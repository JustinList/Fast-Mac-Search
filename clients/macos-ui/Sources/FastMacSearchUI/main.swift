import AppKit
import Carbon
import ServiceManagement
import UniformTypeIdentifiers

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate {
    private let ipc = SearchdIPCClient()
    private var supervisor: DaemonSupervisor!
    private var panel: SpotlightPanel!
    private var overlayController: SearchOverlayController!
    private var driveManager: DriveManagerWindowController!
    private var conversationViewer: ConversationViewerWindowController!

    private var statusItem: NSStatusItem!
    private let statusMenu = NSMenu()
    private var hotKeyRef: EventHotKeyRef?
    private var globalClickMonitor: Any?

    private var latestStatus: EngineStatusPayload?
    private var isRefreshingStatus = false
    private var lastIdlePollDate: Date = .distantPast
    private var pollTimer: Timer?
    private var pulseTimer: Timer?
    private var pulsePhase: Double = 0.0

    func applicationDidFinishLaunching(_ notification: Notification) {
        // 1. Automatically start embedded `searchd` if not already running
        supervisor = DaemonSupervisor(ipc: ipc)
        supervisor.ensureDaemonRunning()

        // 2. Build floating 62pt collapsible Spotlight bar
        let screenFrame = NSScreen.main?.visibleFrame ?? NSRect(x: 0, y: 0, width: 1440, height: 900)
        let w = SearchOverlayController.barWidth
        let h = SearchOverlayController.collapsedHeight
        let originX = screenFrame.midX - w / 2
        let originY = screenFrame.maxY - h - 150

        overlayController = SearchOverlayController(ipc: ipc)
        panel = SpotlightPanel(contentRect: NSRect(x: originX, y: originY, width: w, height: h))
        panel.contentViewController = overlayController

        panel.onResignKeyDismiss = { [weak self] in
            self?.hideOverlay()
        }
        overlayController.onRequestDismiss = { [weak self] in
            self?.hideOverlay()
        }
        overlayController.onOpenDriveManager = { [weak self] in
            self?.openDriveManager()
        }

        // 3. Build visual Drives Manager + Encrypted AI Conversation Reader windows
        driveManager = DriveManagerWindowController(
            ipc: ipc,
            onTriggerFolderPicker: { [weak self] in self?.presentFolderPicker() },
            onRequestRefresh: { [weak self] in self?.refreshDaemonStatusNow() }
        )
        conversationViewer = ConversationViewerWindowController(ipc: ipc)
        overlayController.onOpenConversation = { [weak self] sessionId, highlightQuery in
            self?.conversationViewer.presentConversation(sessionId: sessionId, highlightQuery: highlightQuery)
        }

        setupStatusBar()
        registerGlobalOptionSpaceHotKey()
        setupEventMonitors()
        observeVolumeMounts()
        startStatusPolling()

        // Refresh initial status; if zero drives & zero AI chats indexed on very first launch, show Drive Manager;
        // otherwise stay quietly tucked in the Menu Bar until the user presses ⌥Space or clicks ⚡!
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.35) { [weak self] in
            guard let self = self else { return }
            self.refreshDaemonStatusNow { status in
                if (status?.total_paths ?? 0) == 0 && (status?.llm_sessions_total ?? 0) == 0 {
                    self.openDriveManager()
                }
            }
        }
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        showOverlay()
        return true
    }

    func applicationWillTerminate(_ notification: Notification) {
        if let monitor = globalClickMonitor {
            NSEvent.removeMonitor(monitor)
        }
        supervisor.stopManagedDaemonIfOwned()
    }

    // MARK: - Menu Bar Icon, Pulsing Lightning Animation & Hover Tooltip

    private func setupStatusBar() {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        statusItem.autosaveName = "FastMacSearchStatusItem"
        if let btn = statusItem.button {
            let img = NSImage(systemSymbolName: "bolt.fill", accessibilityDescription: "Fast Mac Search")
            img?.isTemplate = true
            btn.image = img
            btn.toolTip = "Fast Mac Search — Starting encrypted engine… (⌥Space)"
        }
        statusMenu.delegate = self
        statusItem.menu = statusMenu
    }

    private func startStatusPolling() {
        // Adaptive poll: fast (0.5s) while indexing or warming up; calm (2.5s) when idle.
        pollTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            Task { @MainActor in
                guard let self = self else { return }
                let activelyIndexing = self.latestStatus?.indexing.is_indexing ?? true
                if !activelyIndexing && Date().timeIntervalSince(self.lastIdlePollDate) < 2.5 {
                    return
                }
                self.lastIdlePollDate = Date()
                self.refreshDaemonStatusNow()
            }
        }
    }

    private func refreshDaemonStatusNow(completion: (@MainActor (EngineStatusPayload?) -> Void)? = nil) {
        guard !isRefreshingStatus else {
            completion?(latestStatus)
            return
        }
        isRefreshingStatus = true
        let client = self.ipc
        DispatchQueue.global(qos: .utility).async {
            let status = try? client.status()
            DispatchQueue.main.async { [weak self] in
                guard let self = self else { return }
                self.isRefreshingStatus = false
                if status == nil {
                    self.supervisor.ensureDaemonRunning()
                }
                self.latestStatus = status
                self.applyTelemetryToMenuBar(status)
                if self.driveManager.window?.isVisible == true {
                    self.driveManager.updateFromStatus(status)
                }
                completion?(status)
            }
        }
    }

    private func applyTelemetryToMenuBar(_ status: EngineStatusPayload?) {
        guard let btn = statusItem.button else { return }
        guard let st = status else {
            stopPulsingAnimation()
            btn.toolTip = "Fast Mac Search — Engine warming up… (⌥Space)"
            return
        }

        if st.indexing.is_indexing {
            startPulsingAnimationIfNeeded()
            let scanned = NumberFormatter.localizedString(from: NSNumber(value: st.indexing.files_scanned), number: .decimal)
            if st.indexing.entries_total > 0 {
                let done = NumberFormatter.localizedString(from: NSNumber(value: st.indexing.entries_ingested), number: .decimal)
                let tot = NumberFormatter.localizedString(from: NSNumber(value: st.indexing.entries_total), number: .decimal)
                btn.toolTip = "⚡ Indexing \(st.indexing.active_target) (\(st.indexing.phase)): \(done) / \(tot) items (\(scanned) files)…"
            } else {
                btn.toolTip = "⚡ \(st.indexing.phase) (\(st.indexing.active_target))…"
            }
        } else {
            stopPulsingAnimation()
            let total = NumberFormatter.localizedString(from: NSNumber(value: st.total_paths), number: .decimal)
            let chats = NumberFormatter.localizedString(from: NSNumber(value: st.llm_sessions_total), number: .decimal)
            btn.toolTip = "Fast Mac Search — \(total) files + \(chats) AI chats encrypted · Press ⌥Space to search"
        }
    }

    private func startPulsingAnimationIfNeeded() {
        guard pulseTimer == nil else { return }
        if let btn = statusItem.button {
            let img = NSImage(systemSymbolName: "bolt.circle.fill", accessibilityDescription: "Indexing in progress")
            img?.isTemplate = true
            btn.image = img
        }
        pulseTimer = Timer.scheduledTimer(withTimeInterval: 0.06, repeats: true) { [weak self] _ in
            Task { @MainActor in
                guard let self = self, let btn = self.statusItem.button else { return }
                self.pulsePhase += 0.28
                let alpha = 0.65 + 0.35 * cos(self.pulsePhase)
                btn.alphaValue = CGFloat(alpha)
            }
        }
    }

    private func stopPulsingAnimation() {
        guard pulseTimer != nil else { return }
        pulseTimer?.invalidate()
        pulseTimer = nil
        pulsePhase = 0.0
        if let btn = statusItem.button {
            btn.alphaValue = 1.0
            let img = NSImage(systemSymbolName: "bolt.fill", accessibilityDescription: "Fast Mac Search")
            img?.isTemplate = true
            btn.image = img
        }
    }

    // MARK: - Dynamic Menu Bar Dropdown (Files + Cross-LLM Memory + MCP)

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()

        let toggleTitle = panel.isVisible ? "Hide Search Bar" : "Open Search Bar"
        let openItem = NSMenuItem(title: toggleTitle, action: #selector(toggleOverlay), keyEquivalent: " ")
        openItem.keyEquivalentModifierMask = [.option]
        menu.addItem(openItem)

        let aiSearchItem = NSMenuItem(title: "Search AI Conversations…", action: #selector(openAiChatsOverlay), keyEquivalent: "3")
        aiSearchItem.keyEquivalentModifierMask = [.command]
        aiSearchItem.target = self
        menu.addItem(aiSearchItem)

        menu.addItem(.separator())

        // Live Indexing / Status Rows
        if let st = latestStatus {
            if st.indexing.is_indexing {
                let scanned = NumberFormatter.localizedString(from: NSNumber(value: st.indexing.files_scanned), number: .decimal)
                let label: String
                if st.indexing.entries_total > 0 {
                    let done = NumberFormatter.localizedString(from: NSNumber(value: st.indexing.entries_ingested), number: .decimal)
                    let tot = NumberFormatter.localizedString(from: NSNumber(value: st.indexing.entries_total), number: .decimal)
                    label = "⚡ Indexing \(st.indexing.active_target): \(done) / \(tot) items…"
                } else if st.indexing.files_scanned > 0 {
                    label = "⚡ Scanning \(st.indexing.active_target): \(scanned) files…"
                } else {
                    label = "⚡ \(st.indexing.phase)…"
                }
                let progressItem = NSMenuItem(title: label, action: nil, keyEquivalent: "")
                progressItem.isEnabled = false
                menu.addItem(progressItem)
            } else {
                let total = NumberFormatter.localizedString(from: NSNumber(value: st.total_paths), number: .decimal)
                let readyItem = NSMenuItem(
                    title: "✓ \(total) files encrypted (\(st.shard_count) drive shard\(st.shard_count == 1 ? "" : "s"))",
                    action: nil,
                    keyEquivalent: ""
                )
                readyItem.isEnabled = false
                menu.addItem(readyItem)
            }

            let chatCount = NumberFormatter.localizedString(from: NSNumber(value: st.llm_sessions_total), number: .decimal)
            let provBreakdown = st.llm_providers
                .map { "\($0.provider): \($0.count)" }
                .joined(separator: ", ")
            let llmTitle = provBreakdown.isEmpty
                ? "✦ \(chatCount) AI chats encrypted"
                : "✦ \(chatCount) AI chats (\(provBreakdown))"
            let llmStatusItem = NSMenuItem(title: llmTitle, action: nil, keyEquivalent: "")
            llmStatusItem.isEnabled = false
            menu.addItem(llmStatusItem)
        } else {
            let starting = NSMenuItem(title: "Starting background searchd…", action: nil, keyEquivalent: "")
            starting.isEnabled = false
            menu.addItem(starting)
        }

        menu.addItem(.separator())

        // Cross-LLM Memory & MCP Section
        let aiHeader = NSMenuItem(title: "Cross-LLM Memory & MCP", action: nil, keyEquivalent: "")
        aiHeader.isEnabled = false
        menu.addItem(aiHeader)

        let syncChatsItem = NSMenuItem(title: "Sync Local AI Chats Now", action: #selector(handleSyncLlmChats), keyEquivalent: "r")
        syncChatsItem.target = self
        menu.addItem(syncChatsItem)

        let importExportItem = NSMenuItem(title: "Import ChatGPT / Claude Export (.json)…", action: #selector(presentLlmExportImporter), keyEquivalent: "i")
        importExportItem.target = self
        menu.addItem(importExportItem)

        let mcpInstallItem = NSMenuItem(title: "Enable MCP Server for Claude Desktop…", action: #selector(handleInstallMcpServer), keyEquivalent: "")
        mcpInstallItem.target = self
        menu.addItem(mcpInstallItem)

        menu.addItem(.separator())

        // Connected Drives Quick-Add Section
        let drivesHeader = NSMenuItem(title: "Drives (Click to Index / Refresh)", action: nil, keyEquivalent: "")
        drivesHeader.isEnabled = false
        menu.addItem(drivesHeader)

        let drives = DriveManagerWindowController.discoverDrives(merging: latestStatus)
        for drive in drives {
            let title: String
            if let shard = drive.matchingShard {
                let cnt = NumberFormatter.localizedString(from: NSNumber(value: shard.l0_paths), number: .decimal)
                title = drive.isOnline
                    ? "\(drive.name) — \(cnt) paths"
                    : "\(drive.name) — Offline (\(cnt) cached)"
            } else {
                title = "Index \(drive.name)…"
            }

            let item = NSMenuItem(title: title, action: #selector(handleQuickDriveMenuClick(_:)), keyEquivalent: "")
            item.target = self
            item.representedObject = drive.mountPath
            item.state = (drive.matchingShard != nil) ? .on : .off
            item.isEnabled = drive.isOnline
            menu.addItem(item)
        }

        menu.addItem(.separator())

        let addFolderItem = NSMenuItem(title: "Add Folder or Drive…", action: #selector(presentFolderPicker), keyEquivalent: "o")
        addFolderItem.target = self
        menu.addItem(addFolderItem)

        let manageItem = NSMenuItem(title: "Manage Drives & Encrypted Shards…", action: #selector(openDriveManager), keyEquivalent: ",")
        manageItem.target = self
        menu.addItem(manageItem)

        menu.addItem(.separator())

        // Native macOS Launch at Login toggle
        let loginItem = NSMenuItem(title: "Launch at Login", action: #selector(toggleLaunchAtLogin), keyEquivalent: "")
        loginItem.target = self
        loginItem.state = (SMAppService.mainApp.status == .enabled) ? .on : .off
        menu.addItem(loginItem)

        menu.addItem(.separator())

        menu.addItem(NSMenuItem(title: "Quit Fast Mac Search", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q"))
    }

    @objc private func openAiChatsOverlay() {
        showOverlay()
        overlayController.selectScope(index: 2)
    }

    @objc private func handleSyncLlmChats() {
        ipc.syncLlmChatsAsync()
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) { [weak self] in
            self?.refreshDaemonStatusNow()
        }
    }

    @objc private func presentLlmExportImporter() {
        hideOverlay()
        NSApp.activate(ignoringOtherApps: true)

        let openPanel = NSOpenPanel()
        openPanel.title = "Import ChatGPT or Claude Conversations Export"
        openPanel.message = "Select your `conversations.json` export file. Conversations are parsed directly into AES-256-GCM encrypted memory."
        openPanel.prompt = "Encrypt & Import"
        openPanel.canChooseFiles = true
        openPanel.canChooseDirectories = false
        openPanel.allowsMultipleSelection = false
        openPanel.allowedContentTypes = [.json]

        guard openPanel.runModal() == .OK, let url = openPanel.url else { return }
        let client = self.ipc
        let filePath = url.path

        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            do {
                let rep = try client.importLlmExport(path: filePath)
                DispatchQueue.main.async { [weak self] in
                    self?.refreshDaemonStatusNow()
                    let alert = NSAlert()
                    alert.messageText = "AI Conversations Encrypted & Indexed"
                    alert.informativeText = "Imported \(rep.files_parsed) conversations in \(String(format: "%.1f", rep.elapsed_ms)) ms (\(rep.sessions_total) total AI chats now searchable via ⌥Space)."
                    alert.alertStyle = .informational
                    alert.runModal()
                }
            } catch {
                DispatchQueue.main.async {
                    let alert = NSAlert()
                    alert.messageText = "Could Not Import Export File"
                    alert.informativeText = error.localizedDescription
                    alert.alertStyle = .warning
                    alert.runModal()
                }
            }
        }
    }

    @objc private func handleInstallMcpServer() {
        hideOverlay()
        NSApp.activate(ignoringOtherApps: true)

        let bundledFms = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/fms").path
        let fallbackFms = "/Applications/Fast Mac Search.app/Contents/Helpers/fms"
        let fmsPath = FileManager.default.isExecutableFile(atPath: bundledFms) ? bundledFms : fallbackFms

        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let claudeConfigPath = "\(home)/Library/Application Support/Claude/claude_desktop_config.json"
        let parentDir = (claudeConfigPath as NSString).deletingLastPathComponent
        try? FileManager.default.createDirectory(atPath: parentDir, withIntermediateDirectories: true)

        var rootDict: [String: Any] = [:]
        if let existingData = FileManager.default.contents(atPath: claudeConfigPath),
           let parsed = try? JSONSerialization.jsonObject(with: existingData) as? [String: Any]
        {
            rootDict = parsed
        }
        var mcpServers = (rootDict["mcpServers"] as? [String: Any]) ?? [:]
        mcpServers["fast-mac-search"] = [
            "command": fmsPath,
            "args": ["mcp"]
        ]
        rootDict["mcpServers"] = mcpServers

        if let outData = try? JSONSerialization.data(withJSONObject: rootDict, options: [.prettyPrinted, .sortedKeys]) {
            try? outData.write(to: URL(fileURLWithPath: claudeConfigPath), options: .atomic)
        }

        let cliSnippet = "claude mcp add fast-mac-search -- \"\(fmsPath)\" mcp"
        let alert = NSAlert()
        alert.messageText = "MCP Server Enabled for Claude Desktop"
        alert.informativeText = """
        Registered `fast-mac-search` in:
        \(claudeConfigPath)

        Restart Claude Desktop to let Claude search your encrypted files and cross-LLM history (< 2 ms).

        For Claude Code or Codex CLI, you can also run:
        \(cliSnippet)
        """
        alert.addButton(withTitle: "Copy CLI Command")
        alert.addButton(withTitle: "Done")
        if alert.runModal() == .alertFirstButtonReturn {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(cliSnippet, forType: .string)
        }
    }

    @objc private func handleQuickDriveMenuClick(_ sender: NSMenuItem) {
        guard let mountPath = sender.representedObject as? String else { return }
        let targetPath = (mountPath == "/")
            ? FileManager.default.homeDirectoryForCurrentUser.path
            : mountPath
        ipc.startIndexing(rootPath: targetPath, portable: false)
        refreshDaemonStatusNow()
    }

    @objc private func presentFolderPicker() {
        hideOverlay()
        NSApp.activate(ignoringOtherApps: true)

        let openPanel = NSOpenPanel()
        openPanel.title = "Choose a Drive or Folder to Encrypt & Index"
        openPanel.message = "Fast Mac Search will index filenames and file contents into an AES-256-GCM encrypted shard."
        openPanel.prompt = "Index Location"
        openPanel.canChooseFiles = false
        openPanel.canChooseDirectories = true
        openPanel.allowsMultipleSelection = true
        openPanel.treatsFilePackagesAsDirectories = false

        if openPanel.runModal() == .OK {
            for url in openPanel.urls {
                ipc.startIndexing(rootPath: url.path, portable: false)
            }
            refreshDaemonStatusNow()
        }
    }

    @objc private func openDriveManager() {
        hideOverlay()
        driveManager.presentAndRefresh(with: latestStatus)
    }

    @objc private func toggleLaunchAtLogin() {
        do {
            if SMAppService.mainApp.status == .enabled {
                try SMAppService.mainApp.unregister()
            } else {
                try SMAppService.mainApp.register()
            }
        } catch {
            NSSound.beep()
        }
    }

    // MARK: - Search Bar Easy Show / Hide + Click-Outside Dismissal

    @objc func toggleOverlay() {
        if panel.isVisible {
            hideOverlay()
        } else {
            showOverlay()
        }
    }

    private func showOverlay() {
        let frontBundle = NSWorkspace.shared.frontmostApplication?.bundleIdentifier
        if let screen = NSScreen.main?.visibleFrame {
            let x = screen.midX - SearchOverlayController.barWidth / 2
            let currentH = panel.frame.height
            let topEdgeY = screen.maxY - 150
            panel.setFrameOrigin(NSPoint(x: x, y: topEdgeY - currentH))
        }
        panel.alphaValue = 1.0
        panel.makeKeyAndOrderFront(nil)
        overlayController.prepareForPresentation(previousAppBundleId: frontBundle)
    }

    private func hideOverlay() {
        guard panel.isVisible else { return }
        panel.orderOut(nil)
    }

    private func setupEventMonitors() {
        // 1. Local key monitor for ⌘1/⌘2/⌘3 scope switching, ⌘Y preview/reader, & ⌘, Drives Manager
        NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self = self else { return event }
            if event.modifierFlags.contains(.command) {
                let ch = event.charactersIgnoringModifiers?.lowercased()
                if self.panel.isVisible {
                    if ch == "1" {
                        self.overlayController.selectScope(index: 0)
                        return nil
                    }
                    if ch == "2" {
                        self.overlayController.selectScope(index: 1)
                        return nil
                    }
                    if ch == "3" {
                        self.overlayController.selectScope(index: 2)
                        return nil
                    }
                    if ch == "y" {
                        self.overlayController.toggleQuickLook()
                        return nil
                    }
                }
                if ch == "," {
                    self.openDriveManager()
                    return nil
                }
            }
            return event
        }

        // 2. Global mouse-down monitor: clicking ANYWHERE outside the Search Bar immediately dismisses it!
        globalClickMonitor = NSEvent.addGlobalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown]) { [weak self] _ in
            Task { @MainActor in
                guard let self = self, self.panel.isVisible else { return }
                let mouseLoc = NSEvent.mouseLocation
                if !self.panel.frame.contains(mouseLoc) {
                    self.hideOverlay()
                }
            }
        }
    }

    private func observeVolumeMounts() {
        let wsCenter = NSWorkspace.shared.notificationCenter
        wsCenter.addObserver(forName: NSWorkspace.didMountNotification, object: nil, queue: .main) { [weak self] _ in
            Task { @MainActor in
                self?.refreshDaemonStatusNow()
            }
        }
        wsCenter.addObserver(forName: NSWorkspace.didUnmountNotification, object: nil, queue: .main) { [weak self] _ in
            Task { @MainActor in
                self?.refreshDaemonStatusNow()
            }
        }
    }

    private func registerGlobalOptionSpaceHotKey() {
        var eventType = EventTypeSpec(
            eventClass: OSType(kEventClassKeyboard),
            eventKind: UInt32(kEventHotKeyPressed)
        )
        let selfPtr = Unmanaged.passUnretained(self).toOpaque()

        InstallEventHandler(
            GetApplicationEventTarget(),
            { (_, _, userData) -> OSStatus in
                guard let userData = userData else { return noErr }
                let delegate = Unmanaged<AppDelegate>.fromOpaque(userData).takeUnretainedValue()
                Task { @MainActor in
                    delegate.toggleOverlay()
                }
                return noErr
            },
            1,
            &eventType,
            selfPtr,
            nil
        )

        let hotKeyID = EventHotKeyID(signature: OSType(0x464D_5331), id: 1) // 'FMS1'
        RegisterEventHotKey(
            UInt32(kVK_Space),
            UInt32(optionKey),
            hotKeyID,
            GetApplicationEventTarget(),
            0,
            &hotKeyRef
        )
    }
}

// Ignore SIGPIPE globally so broken AF_UNIX sockets never silently terminate the macOS UI process.
Darwin.signal(SIGPIPE, SIG_IGN)

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let delegate = AppDelegate()
app.delegate = delegate
app.run()
