import AppKit
import Quartz
import UniformTypeIdentifiers
import WebKit

/// Non-activating floating vibrancy Spotlight overlay panel that joins all macOS Spaces,
/// renders over fullscreen apps at 120Hz ProMotion, and auto-dismisses when clicking outside.
@MainActor
public final class SpotlightPanel: NSPanel {
    public var onResignKeyDismiss: (() -> Void)?

    public init(contentRect: NSRect) {
        super.init(
            contentRect: contentRect,
            styleMask: [.borderless, .nonactivatingPanel, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )
        self.isFloatingPanel = true
        self.level = .statusBar
        self.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .transient]
        self.backgroundColor = .clear
        self.isOpaque = false
        self.hasShadow = true
        self.titleVisibility = .hidden
        self.titlebarAppearsTransparent = true
        self.isMovableByWindowBackground = true
    }

    public override var canBecomeKey: Bool { true }
    public override var canBecomeMain: Bool { false }

    public override func resignKey() {
        super.resignKey()
        if QLPreviewPanel.sharedPreviewPanelExists(), QLPreviewPanel.shared().isVisible {
            return
        }
        onResignKeyDismiss?()
    }
}

private enum WebRowKind: Sendable {
    case searchGoogle(query: String)
    case googleSuggestion(typed: String, suggestion: String)
    case askGrok(query: String)
    case searchX(query: String)

    var title: String {
        switch self {
        case .searchGoogle(let q): return "Search Google for “\(q)”"
        case .googleSuggestion(_, let s): return s
        case .askGrok(let q): return "Ask Grok “\(q)”"
        case .searchX(let q): return "Search X for “\(q)”"
        }
    }

    var badge: String {
        switch self {
        case .searchGoogle, .googleSuggestion: return "Google"
        case .askGrok: return "Grok"
        case .searchX: return "X"
        }
    }

    var symbolName: String {
        switch self {
        case .searchGoogle: return "globe"
        case .googleSuggestion: return "magnifyingglass"
        case .askGrok: return "sparkles"
        case .searchX: return "number"
        }
    }

    var targetURL: URL? {
        switch self {
        case .searchGoogle(let q): return WebSuggestClient.googleSearchURL(for: q)
        case .googleSuggestion(_, let s): return WebSuggestClient.googleSearchURL(for: s)
        case .askGrok(let q): return WebSuggestClient.askGrokURL(for: q)
        case .searchX(let q): return WebSuggestClient.searchXURL(for: q)
        }
    }

    var opensInPanelPreview: Bool {
        switch self {
        case .searchGoogle, .googleSuggestion: return true
        case .askGrok, .searchX: return false
        }
    }
}

private enum OverlayRowItem {
    case sectionHeader(String)
    case calculator(CalcAnswerPayload)
    case app(RankedAppHit, isSuggested: Bool, shortcutNumber: Int?)
    case searchHit(RankedHitPayload, matchedIndices: [Int], shortcutNumber: Int?)
    case web(WebRowKind, shortcutNumber: Int?)

    var isSelectable: Bool {
        if case .sectionHeader = self { return false }
        return true
    }

    var rowHeight: CGFloat {
        switch self {
        case .sectionHeader: return 25
        case .calculator: return 68
        case .app: return 44
        case .searchHit: return 50
        case .web: return 40
        }
    }
}

@MainActor
public final class SearchOverlayController: NSViewController, NSTextFieldDelegate, NSTableViewDataSource, NSTableViewDelegate, QLPreviewPanelDataSource, QLPreviewPanelDelegate, WKNavigationDelegate, WKUIDelegate {
    public static let barWidth: CGFloat = 750
    public static let collapsedHeight: CGFloat = 62
    public static let footerHeight: CGFloat = 34
    public static let maxExpandedHeight: CGFloat = 540
    public static let webPreviewExpandedHeight: CGFloat = 560

    private let ipc: SearchdIPCClient
    private let appIndex = AppLauncherIndex()
    private let webSuggest = WebSuggestClient()

    public var onOpenDriveManager: (() -> Void)?
    public var onOpenConversation: ((String, String) -> Void)?
    public var onRequestDismiss: (() -> Void)?

    private var rows: [OverlayRowItem] = []
    private var latestCalc: CalcAnswerPayload?
    private var latestAppHits: [RankedAppHit] = []
    private var latestDaemonHits: [RankedHitPayload] = []
    private var latestWebSuggestions: [String] = []
    private var latestDaemonGhostSuffix: String?
    private var activeGhostSuffix: String?
    private var latestElapsedMs: Double?
    private var latestIndexedCount: Int = 0

    private var capturedAppBundleId: String?
    private var selectedScopeIndex: Int = 0 // 0 = all, 1 = files, 2 = chats
    private let queryQueue = DispatchQueue(label: "dev.fastmacsearch.ui.query", qos: .userInteractive)
    private var querySeq: UInt64 = 0
    nonisolated(unsafe) private var keyDownMonitor: Any?
    private var footerFlashResetWork: DispatchWorkItem?

    // Views
    private let vibrancyView = NSVisualEffectView()
    private let iconLabel = NSTextField(labelWithString: "⌕")
    private let searchField = NSTextField()
    private let ghostLabel = NSTextField(labelWithString: "")
    private let latencyPill = NSTextField(labelWithString: "⌥Space")
    private let scopeControl = NSSegmentedControl(labels: ["All", "Files", "AI"], trackingMode: .selectOne, target: nil, action: nil)
    private let drivesBtn = NSButton()
    private let closeBtn = NSButton()

    private let headerDivider = NSBox()
    private let scrollView = NSScrollView()
    private let tableView = NSTableView()
    private let selectionPillView = NSView()

    private let footerDivider = NSBox()
    private let footerLeftLabel = NSTextField(labelWithString: "")
    private let footerRightLabel = NSTextField(labelWithString: "")

    // Lazy In-Panel WKWebView Preview (only instantiated if Web Suggestions is toggled ON and used)
    private var webPreviewView: WKWebView?
    private var activeWebPreviewURL: URL?

    public init(ipc: SearchdIPCClient) {
        self.ipc = ipc
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    deinit {
        if let monitor = keyDownMonitor {
            NSEvent.removeMonitor(monitor)
        }
    }

    public override func loadView() {
        let w = Self.barWidth
        let h = Self.collapsedHeight

        vibrancyView.frame = NSRect(x: 0, y: 0, width: w, height: h)
        vibrancyView.autoresizingMask = [.width, .height]
        vibrancyView.material = .hudWindow
        vibrancyView.blendingMode = .behindWindow
        vibrancyView.state = .active
        vibrancyView.wantsLayer = true
        vibrancyView.layer?.cornerRadius = 16
        vibrancyView.layer?.masksToBounds = true
        vibrancyView.layer?.borderWidth = 1.0
        vibrancyView.layer?.borderColor = NSColor.white.withAlphaComponent(0.18).cgColor

        iconLabel.font = .systemFont(ofSize: 21, weight: .medium)
        iconLabel.textColor = .secondaryLabelColor
        vibrancyView.addSubview(iconLabel)

        ghostLabel.font = .systemFont(ofSize: 19, weight: .regular)
        ghostLabel.textColor = .tertiaryLabelColor
        ghostLabel.lineBreakMode = .byClipping
        vibrancyView.addSubview(ghostLabel)

        searchField.placeholderString = "Apps, math (2^10*3), files, or AI chats (@claude, @codex)…"
        searchField.font = .systemFont(ofSize: 19, weight: .regular)
        searchField.isBordered = false
        searchField.drawsBackground = false
        searchField.focusRingType = .none
        searchField.delegate = self
        vibrancyView.addSubview(searchField)

        latencyPill.font = .monospacedSystemFont(ofSize: 11, weight: .medium)
        latencyPill.textColor = .secondaryLabelColor
        latencyPill.alignment = .right
        vibrancyView.addSubview(latencyPill)

        scopeControl.controlSize = .small
        scopeControl.font = .systemFont(ofSize: 11, weight: .medium)
        scopeControl.segmentStyle = .rounded
        scopeControl.selectedSegment = 0
        scopeControl.target = self
        scopeControl.action = #selector(didChangeScopeSegment)
        scopeControl.toolTip = "Scope: All (⌘1) · Files (⌘2) · AI Chats (⌘3)"
        vibrancyView.addSubview(scopeControl)

        drivesBtn.bezelStyle = .inline
        drivesBtn.isBordered = false
        drivesBtn.image = NSImage(systemSymbolName: "externaldrive.badge.plus", accessibilityDescription: "Manage Drives & Folders")
        drivesBtn.contentTintColor = .secondaryLabelColor
        drivesBtn.toolTip = "Add or Manage Indexed Drives & Folders (⌘,)"
        drivesBtn.target = self
        drivesBtn.action = #selector(didTapDrivesButton)
        vibrancyView.addSubview(drivesBtn)

        closeBtn.bezelStyle = .inline
        closeBtn.isBordered = false
        closeBtn.image = NSImage(systemSymbolName: "xmark.circle.fill", accessibilityDescription: "Hide Search Bar")
        closeBtn.contentTintColor = .tertiaryLabelColor
        closeBtn.toolTip = "Hide Search Bar (Esc or ⌥Space)"
        closeBtn.target = self
        closeBtn.action = #selector(didTapCloseButton)
        vibrancyView.addSubview(closeBtn)

        headerDivider.boxType = .separator
        headerDivider.isHidden = true
        vibrancyView.addSubview(headerDivider)

        // Sliding Nanolight glass selection pill inside NSTableView
        selectionPillView.wantsLayer = true
        selectionPillView.layer?.cornerRadius = 9
        selectionPillView.layer?.backgroundColor = NSColor.white.withAlphaComponent(0.13).cgColor
        selectionPillView.layer?.borderWidth = 0.5
        selectionPillView.layer?.borderColor = NSColor.white.withAlphaComponent(0.16).cgColor
        selectionPillView.layer?.zPosition = -1
        selectionPillView.isHidden = true

        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("UnifiedCol"))
        column.width = w - 16
        tableView.addTableColumn(column)
        tableView.headerView = nil
        tableView.rowHeight = 46
        tableView.intercellSpacing = NSSize(width: 0, height: 1)
        tableView.backgroundColor = .clear
        tableView.selectionHighlightStyle = .none
        tableView.style = .plain
        tableView.dataSource = self
        tableView.delegate = self
        tableView.target = self
        tableView.doubleAction = #selector(openSelectedResult)
        tableView.addSubview(selectionPillView)

        scrollView.drawsBackground = false
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.documentView = tableView
        scrollView.isHidden = true
        vibrancyView.addSubview(scrollView)

        footerDivider.boxType = .separator
        footerDivider.isHidden = true
        vibrancyView.addSubview(footerDivider)

        footerLeftLabel.font = .monospacedSystemFont(ofSize: 11, weight: .medium)
        footerLeftLabel.textColor = .secondaryLabelColor
        footerLeftLabel.lineBreakMode = .byTruncatingTail
        footerLeftLabel.isHidden = true
        vibrancyView.addSubview(footerLeftLabel)

        footerRightLabel.font = .systemFont(ofSize: 11.5, weight: .medium)
        footerRightLabel.textColor = .secondaryLabelColor
        footerRightLabel.alignment = .right
        footerRightLabel.lineBreakMode = .byTruncatingHead
        footerRightLabel.isHidden = true
        vibrancyView.addSubview(footerRightLabel)

        self.view = vibrancyView
        installPanelKeyShortcutsMonitor()
        relayoutSubviews(forHeight: h)
    }

    private func installPanelKeyShortcutsMonitor() {
        keyDownMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self = self, self.view.window?.isKeyWindow == true else { return event }
            let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)

            // When In-Panel Web Preview is active: Esc closes preview, ⌘[ goes back, ⌘Return opens in browser
            if self.isShowingWebPreview {
                if event.keyCode == 53 { // Escape
                    self.hideWebPreview()
                    return nil
                }
                if flags == .command && (event.keyCode == 36 || event.keyCode == 76) { // ⌘Return
                    if let url = self.webPreviewView?.url ?? self.activeWebPreviewURL {
                        NSWorkspace.shared.open(url)
                        self.hideWebPreview()
                        self.onRequestDismiss?()
                    }
                    return nil
                }
                if flags == .command && event.charactersIgnoringModifiers == "[" {
                    self.webPreviewView?.goBack()
                    return nil
                }
            }

            // ⌘1 / ⌘2 / ⌘3 switches All | Files | AI scope
            if flags == .command, let chars = event.charactersIgnoringModifiers {
                switch chars {
                case "1":
                    self.selectScope(index: 0)
                    return nil
                case "2":
                    self.selectScope(index: 1)
                    return nil
                case "3":
                    self.selectScope(index: 2)
                    return nil
                case "y", "Y":
                    self.toggleQuickLook()
                    return nil
                case ",":
                    self.didTapDrivesButton()
                    return nil
                default:
                    break
                }
            }

            // ⌥1 – ⌥9 immediately launches actionable row #1–#9!
            if flags == .option, let chars = event.charactersIgnoringModifiers,
               let digit = Int(chars), (1...9).contains(digit)
            {
                if self.triggerShortcutNumber(digit) {
                    return nil
                }
            }

            // ⌃N / ⌃P Emacs-style navigation (like Nanolight)
            if flags == .control, let chars = event.charactersIgnoringModifiers?.lowercased() {
                if chars == "n" {
                    self.moveSelection(delta: 1)
                    return nil
                }
                if chars == "p" {
                    self.moveSelection(delta: -1)
                    return nil
                }
            }

            return event
        }
    }

    private var isShowingWebPreview: Bool {
        if let web = webPreviewView, !web.isHidden {
            return true
        }
        return false
    }

    private func relayoutSubviews(forHeight h: CGFloat) {
        let w = Self.barWidth
        let headerCenterY = h - (Self.collapsedHeight / 2.0)

        iconLabel.frame = NSRect(x: 18, y: headerCenterY - 14, width: 26, height: 28)
        ghostLabel.frame = NSRect(x: 48, y: headerCenterY - 13, width: 358, height: 26)
        searchField.frame = NSRect(x: 48, y: headerCenterY - 13, width: 358, height: 26)
        latencyPill.frame = NSRect(x: 410, y: headerCenterY - 8, width: 124, height: 18)
        scopeControl.frame = NSRect(x: 540, y: headerCenterY - 11, width: 140, height: 22)
        drivesBtn.frame = NSRect(x: w - 62, y: headerCenterY - 12, width: 24, height: 24)
        closeBtn.frame = NSRect(x: w - 34, y: headerCenterY - 11, width: 22, height: 22)

        // Update selection pill contrast dynamically for light/dark appearance
        let isDark = vibrancyView.effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
        selectionPillView.layer?.backgroundColor = (isDark
            ? NSColor.white.withAlphaComponent(0.14)
            : NSColor.black.withAlphaComponent(0.08)).cgColor
        selectionPillView.layer?.borderColor = (isDark
            ? NSColor.white.withAlphaComponent(0.16)
            : NSColor.black.withAlphaComponent(0.08)).cgColor

        let expanded = h > Self.collapsedHeight + 8
        headerDivider.isHidden = !expanded
        footerDivider.isHidden = !expanded
        footerLeftLabel.isHidden = !expanded
        footerRightLabel.isHidden = !expanded

        if !expanded {
            scrollView.isHidden = true
            webPreviewView?.isHidden = true
            return
        }

        headerDivider.frame = NSRect(x: 0, y: h - Self.collapsedHeight, width: w, height: 1)
        footerDivider.frame = NSRect(x: 0, y: Self.footerHeight, width: w, height: 1)
        footerLeftLabel.frame = NSRect(x: 16, y: 9, width: 330, height: 16)
        footerRightLabel.frame = NSRect(x: 350, y: 9, width: w - 366, height: 16)

        let bodyFrame = NSRect(
            x: 0,
            y: Self.footerHeight + 1,
            width: w,
            height: max(0, h - Self.collapsedHeight - Self.footerHeight - 2)
        )

        if isShowingWebPreview {
            scrollView.isHidden = true
            webPreviewView?.frame = bodyFrame
        } else {
            scrollView.isHidden = false
            scrollView.frame = bodyFrame
            if let col = tableView.tableColumns.first {
                col.width = w - 16
            }
        }
    }

    private func resizePanelKeepingTopEdge(targetHeight: CGFloat) {
        guard let win = view.window else { return }
        let maxH = isShowingWebPreview ? Self.webPreviewExpandedHeight : Self.maxExpandedHeight
        let clamped = max(Self.collapsedHeight, min(maxH, targetHeight))
        let oldFrame = win.frame
        if abs(oldFrame.height - clamped) < 1.0 {
            relayoutSubviews(forHeight: clamped)
            return
        }
        let newOriginY = oldFrame.maxY - clamped
        let newFrame = NSRect(x: oldFrame.origin.x, y: newOriginY, width: Self.barWidth, height: clamped)
        win.setFrame(newFrame, display: true, animate: false)
        relayoutSubviews(forHeight: clamped)
    }

    public func selectScope(index: Int) {
        let clamped = max(0, min(2, index))
        selectedScopeIndex = clamped
        scopeControl.selectedSegment = clamped
        hideWebPreview(restoreFocus: false)
        updatePlaceholderForScope()
        runKeystrokeQuery(searchField.stringValue)
    }

    public func refreshAfterPreferencesChange() {
        runKeystrokeQuery(searchField.stringValue)
    }

    private func currentScopeWireValue() -> String {
        switch selectedScopeIndex {
        case 1: return "files"
        case 2: return "chats"
        default: return "all"
        }
    }

    private func updatePlaceholderForScope() {
        switch selectedScopeIndex {
        case 1:
            searchField.placeholderString = "Search files, paths, or document contents…"
        case 2:
            searchField.placeholderString = "Search Claude, Codex, Gemini & ChatGPT chats…"
        default:
            searchField.placeholderString = "Apps, math (2^10*3), files, or AI chats (@claude, @codex)…"
        }
    }

    public func prepareForPresentation(previousAppBundleId: String?) {
        self.capturedAppBundleId = previousAppBundleId
        self.appIndex.rescanIfNeeded(force: false)
        hideWebPreview(restoreFocus: false)
        runKeystrokeQuery(searchField.stringValue)
        self.view.window?.makeFirstResponder(searchField)
        searchField.selectText(nil)
    }

    @objc private func didChangeScopeSegment() {
        selectScope(index: scopeControl.selectedSegment)
        self.view.window?.makeFirstResponder(searchField)
    }

    @objc private func didTapDrivesButton() {
        onRequestDismiss?()
        onOpenDriveManager?()
    }

    @objc private func didTapCloseButton() {
        hideWebPreview(restoreFocus: false)
        onRequestDismiss?()
    }

    public func controlTextDidChange(_ obj: Notification) {
        if isShowingWebPreview {
            hideWebPreview(restoreFocus: false)
        }
        runKeystrokeQuery(searchField.stringValue)
    }

    // MARK: - Unified <0.05ms Local Reflex (Apps + Math) + <0.5ms Encrypted Daemon IPC

    private func runKeystrokeQuery(_ rawText: String) {
        querySeq &+= 1
        let mySeq = querySeq
        let scopeWire = currentScopeWireValue()
        let trimmed = rawText.trimmingCharacters(in: .whitespacesAndNewlines)
        let hasProviderTag = trimmed.hasPrefix("@")

        // 1. Empty Query Behavior:
        //    - `All`: Show Nanolight's `SUGGESTIONS` (most-opened macOS apps)!
        //    - `Files`: Collapse cleanly to compact 62pt bar.
        //    - `AI`: Fetch & list most recent encrypted AI conversations!
        if trimmed.isEmpty {
            latestCalc = nil
            latestWebSuggestions = []
            latestDaemonGhostSuffix = nil
            latestElapsedMs = nil

            if scopeWire == "all" {
                latestAppHits = appIndex.suggestedApps(limit: 5)
                latestDaemonHits = []
                latencyPill.stringValue = "⌥Space"
                rebuildUnifiedRows(forQuery: rawText, isEmptySuggestions: true, animateSelection: false)
                return
            } else if scopeWire == "files" {
                latestAppHits = []
                latestDaemonHits = []
                rows = []
                updateGhostDisplay(rawInput: "", suffix: nil)
                latencyPill.stringValue = "⌥Space"
                selectionPillView.isHidden = true
                tableView.reloadData()
                resizePanelKeepingTopEdge(targetHeight: Self.collapsedHeight)
                return
            }
        }

        // 2. Synchronous <0.05ms Nanolight Reflex (Calculator + macOS Applications + Cached Web Stand-Ins)
        if scopeWire == "all" && !hasProviderTag {
            latestCalc = MathCalculator.evaluate(trimmed)
            latestAppHits = appIndex.rankApps(query: trimmed, limit: 4)
            if latestCalc == nil {
                latestWebSuggestions = webSuggest.completions(for: trimmed) { [weak self] answeredQuery, completions in
                    guard let self = self else { return }
                    let currentTrimmed = self.searchField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
                    guard currentTrimmed.caseInsensitiveCompare(answeredQuery.trimmingCharacters(in: .whitespacesAndNewlines)) == .orderedSame,
                          self.currentScopeWireValue() == "all"
                    else { return }
                    self.latestWebSuggestions = completions
                    self.rebuildUnifiedRows(forQuery: self.searchField.stringValue, isEmptySuggestions: false, preserveSelection: true, animateSelection: false)
                }
            } else {
                latestWebSuggestions = []
            }
        } else {
            latestCalc = nil
            latestAppHits = []
            latestWebSuggestions = []
        }

        // Pure math expression fast-path (just like Nanolight: Calculator + matching Apps only, <0.02 ms!)
        if scopeWire == "all" && latestCalc != nil {
            latestDaemonHits = []
            latestDaemonGhostSuffix = nil
            latencyPill.stringValue = "⚡ <0.02 ms"
            rebuildUnifiedRows(forQuery: rawText, isEmptySuggestions: false, animateSelection: false)
            return
        }

        // Render synchronous App hits IMMEDIATELY before IPC even returns!
        if scopeWire == "all" && !latestAppHits.isEmpty {
            rebuildUnifiedRows(forQuery: rawText, isEmptySuggestions: false, animateSelection: false)
        }

        // 3. Asynchronous ~0.3ms Encrypted `searchd` Query (Files + Cross-LLM Conversations)
        let ctx = QueryContextPayload(activeAppBundleId: capturedAppBundleId)
        let client = self.ipc
        let fetchLimit = (scopeWire == "all") ? 8 : 12

        queryQueue.async { [weak self] in
            do {
                let resp = try client.query(rawText, limit: fetchLimit, scope: scopeWire, context: ctx)
                DispatchQueue.main.async { [weak self] in
                    guard let self = self, self.querySeq == mySeq else { return }

                    self.latestDaemonHits = resp.hits
                    self.latestDaemonGhostSuffix = resp.ghost_prediction?.ghost_suffix
                    if self.latestCalc == nil, let daemonCalc = resp.calc_answer {
                        self.latestCalc = daemonCalc
                    }

                    let ms = Double(resp.total_elapsed_us) / 1000.0
                    self.latestElapsedMs = ms
                    self.latestIndexedCount = resp.total_indexed_paths

                    if resp.total_indexed_paths == 0 && self.latestAppHits.isEmpty && self.latestCalc == nil {
                        self.latencyPill.stringValue = "Index drive ↗"
                    } else {
                        self.latencyPill.stringValue = String(format: "⚡ %.2f ms", ms)
                    }

                    self.rebuildUnifiedRows(
                        forQuery: rawText,
                        isEmptySuggestions: false,
                        animateSelection: false
                    )
                }
            } catch {
                DispatchQueue.main.async { [weak self] in
                    guard let self = self, self.querySeq == mySeq else { return }
                    self.latencyPill.stringValue = self.latestAppHits.isEmpty ? "Starting…" : "Apps Ready"
                    self.rebuildUnifiedRows(forQuery: rawText, isEmptySuggestions: false, animateSelection: false)
                }
            }
        }
    }

    private func rebuildUnifiedRows(
        forQuery rawQuery: String,
        isEmptySuggestions: Bool,
        preserveSelection: Bool = false,
        animateSelection: Bool = false
    ) {
        let previousSelectedRow = tableView.selectedRow
        let trimmed = rawQuery.trimmingCharacters(in: .whitespacesAndNewlines)

        // Strip `@provider` tag when computing matched-letter highlights on titles
        let highlightNeedle: String = {
            if trimmed.hasPrefix("@"), let spaceIdx = trimmed.firstIndex(of: " ") {
                return String(trimmed[trimmed.index(after: spaceIdx)...]).trimmingCharacters(in: .whitespaces)
            }
            return trimmed
        }()

        var built: [OverlayRowItem] = []
        var nextShortcut = 1
        func claimShortcut() -> Int? {
            guard nextShortcut <= 9 else { return nil }
            defer { nextShortcut += 1 }
            return nextShortcut
        }

        // Section 1: Calculator
        if let calc = latestCalc {
            built.append(.sectionHeader("CALCULATOR"))
            built.append(.calculator(calc))
        }

        // Section 2: Applications (or Empty-Query Frequent App Suggestions)
        if !latestAppHits.isEmpty {
            built.append(.sectionHeader(isEmptySuggestions ? "SUGGESTIONS" : "APPLICATIONS"))
            for appHit in latestAppHits {
                built.append(.app(appHit, isSuggested: isEmptySuggestions, shortcutNumber: claimShortcut()))
            }
        }

        // Separate Daemon Hits into AI Conversations vs Files & Folders for clean Nanolight sections
        let aiHits = latestDaemonHits.filter { $0.is_llm_chat }
        let fileHits = latestDaemonHits.filter { !$0.is_llm_chat }

        // Section 3: AI Conversations
        if !aiHits.isEmpty {
            built.append(.sectionHeader(trimmed.isEmpty ? "RECENT AI CONVERSATIONS" : "AI CONVERSATIONS"))
            for hit in aiHits {
                let indices = highlightNeedle.isEmpty
                    ? []
                    : (LauncherMatcher.score(query: highlightNeedle, name: hit.filename)?.matchedCharIndices ?? [])
                built.append(.searchHit(hit, matchedIndices: indices, shortcutNumber: claimShortcut()))
            }
        }

        // Section 4: Encrypted Files & Folders
        if !fileHits.isEmpty {
            built.append(.sectionHeader("FILES & FOLDERS"))
            for hit in fileHits {
                let indices = highlightNeedle.isEmpty
                    ? []
                    : (LauncherMatcher.score(query: highlightNeedle, name: hit.filename)?.matchedCharIndices ?? [])
                built.append(.searchHit(hit, matchedIndices: indices, shortcutNumber: claimShortcut()))
            }
        }

        // Section 5: Opt-In Privacy-Guarded Web Suggestions + Ask Grok / Search X
        // Strictly shown ONLY when WebSuggestions is toggled ON in ⚡ menu and query isn't math or @tag!
        if WebSuggestClient.isEnabled,
           currentScopeWireValue() == "all",
           !trimmed.isEmpty,
           !trimmed.hasPrefix("@"),
           latestCalc == nil
        {
            built.append(.sectionHeader("GOOGLE"))
            built.append(.web(.searchGoogle(query: trimmed), shortcutNumber: claimShortcut()))
            for sug in latestWebSuggestions.prefix(3) {
                built.append(.web(.googleSuggestion(typed: trimmed, suggestion: sug), shortcutNumber: claimShortcut()))
            }
            built.append(.sectionHeader("ASK & SEARCH"))
            built.append(.web(.askGrok(query: trimmed), shortcutNumber: claimShortcut()))
            built.append(.web(.searchX(query: trimmed), shortcutNumber: claimShortcut()))
        }

        self.rows = built
        computeInlineGhostCompletion(rawInput: rawQuery)

        tableView.reloadData()

        if rows.isEmpty {
            selectionPillView.isHidden = true
            resizePanelKeepingTopEdge(targetHeight: Self.collapsedHeight)
            return
        }

        // Calculate exact content height + header + Nanolight action footer
        var contentH: CGFloat = 6
        for item in rows {
            contentH += item.rowHeight + tableView.intercellSpacing.height
        }
        let desiredH = Self.collapsedHeight + contentH + Self.footerHeight + 4
        resizePanelKeepingTopEdge(targetHeight: desiredH)

        // Select first actionable row (or preserve row index when async web completions arrive)
        var targetRow: Int?
        if preserveSelection,
           previousSelectedRow >= 0,
           previousSelectedRow < rows.count,
           rows[previousSelectedRow].isSelectable
        {
            targetRow = previousSelectedRow
        } else {
            targetRow = rows.firstIndex(where: { $0.isSelectable })
        }

        if let sel = targetRow {
            tableView.selectRowIndexes(IndexSet(integer: sel), byExtendingSelection: false)
            updateSlidingSelectionPill(animated: animateSelection)
        } else {
            selectionPillView.isHidden = true
        }
        updateFooterLabels()
    }

    /// Computes pixel-aligned inline ghost suffix:
    /// 1. Top matching Application name starting with typed query (`"saf"` -> `"ari"`)
    /// 2. System 1 Markov/file ghost prediction
    /// 3. Top Google suggestion (if Web Suggestions enabled)
    private func computeInlineGhostCompletion(rawInput: String) {
        guard latestCalc == nil, !rawInput.isEmpty, rawInput == rawInput.trimmingCharacters(in: .newlines) else {
            updateGhostDisplay(rawInput: "", suffix: nil)
            return
        }

        let lowerInput = rawInput.lowercased()

        // Priority 1: Top App prefix completion
        if let topApp = latestAppHits.first?.app.name,
           topApp.count > rawInput.count,
           topApp.lowercased().hasPrefix(lowerInput)
        {
            let suffixIdx = topApp.index(topApp.startIndex, offsetBy: rawInput.count)
            updateGhostDisplay(rawInput: rawInput, suffix: String(topApp[suffixIdx...]))
            return
        }

        // Priority 2: System 1 file/query ghost prediction
        if let s1Suffix = latestDaemonGhostSuffix, !s1Suffix.isEmpty {
            updateGhostDisplay(rawInput: rawInput, suffix: s1Suffix)
            return
        }

        // Priority 3: Top Google completion (only when Web Suggestions enabled)
        if WebSuggestClient.isEnabled,
           let topSug = latestWebSuggestions.first,
           topSug.count > rawInput.count,
           topSug.lowercased().hasPrefix(lowerInput)
        {
            let suffixIdx = topSug.index(topSug.startIndex, offsetBy: rawInput.count)
            updateGhostDisplay(rawInput: rawInput, suffix: String(topSug[suffixIdx...]))
            return
        }

        updateGhostDisplay(rawInput: rawInput, suffix: nil)
    }

    private func updateGhostDisplay(rawInput: String, suffix: String?) {
        guard let suffix = suffix, !suffix.isEmpty, !rawInput.isEmpty else {
            activeGhostSuffix = nil
            ghostLabel.attributedStringValue = NSAttributedString(string: "")
            return
        }
        activeGhostSuffix = suffix
        let font = searchField.font ?? .systemFont(ofSize: 19, weight: .regular)

        // Render `rawInput` in `.clear` with identical font so glyph kerning & advance match 100%
        let attr = NSMutableAttributedString(
            string: rawInput,
            attributes: [
                .font: font,
                .foregroundColor: NSColor.clear
            ]
        )
        attr.append(
            NSAttributedString(
                string: suffix,
                attributes: [
                    .font: font,
                    .foregroundColor: NSColor.tertiaryLabelColor
                ]
            )
        )
        ghostLabel.attributedStringValue = attr
    }

    private func acceptInlineGhostIfAvailable() -> Bool {
        guard let suffix = activeGhostSuffix, !suffix.isEmpty else { return false }
        let completed = searchField.stringValue + suffix
        searchField.stringValue = completed
        updateGhostDisplay(rawInput: completed, suffix: nil)
        if let editor = searchField.currentEditor() {
            editor.selectedRange = NSRange(location: (completed as NSString).length, length: 0)
        }
        runKeystrokeQuery(completed)
        return true
    }

    // MARK: - Smooth Sliding Selection Pill & Dynamic Nanolight Action Footer

    private func updateSlidingSelectionPill(animated: Bool) {
        let row = tableView.selectedRow
        guard row >= 0, row < rows.count, rows[row].isSelectable else {
            selectionPillView.isHidden = true
            return
        }
        let rawRect = tableView.rect(ofRow: row)
        guard rawRect.height > 0 else {
            selectionPillView.isHidden = true
            return
        }
        let pillFrame = rawRect.insetBy(dx: 8, dy: 2)
        if selectionPillView.isHidden || !animated {
            selectionPillView.frame = pillFrame
            selectionPillView.isHidden = false
        } else {
            NSAnimationContext.runAnimationGroup { ctx in
                ctx.duration = 0.11
                ctx.timingFunction = CAMediaTimingFunction(name: .easeOut)
                self.selectionPillView.animator().frame = pillFrame
            }
        }
    }

    private func flashFooterMessage(_ text: String) {
        footerFlashResetWork?.cancel()
        footerLeftLabel.textColor = .systemGreen
        footerLeftLabel.stringValue = text

        let work = DispatchWorkItem { [weak self] in
            Task { @MainActor in
                guard let self = self else { return }
                self.footerLeftLabel.textColor = .secondaryLabelColor
                self.updateFooterLabels()
            }
        }
        footerFlashResetWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.6, execute: work)
    }

    private func updateFooterLabels() {
        if isShowingWebPreview {
            footerLeftLabel.textColor = .secondaryLabelColor
            footerLeftLabel.stringValue = activeWebPreviewURL?.host ?? "Web Preview"
            footerRightLabel.stringValue = "⎋ Back to List   ⌘[ Back   ⌘↵ Open in Browser"
            return
        }

        if footerLeftLabel.textColor != .systemGreen {
            if let ms = latestElapsedMs {
                let countFmt = NumberFormatter.localizedString(from: NSNumber(value: latestIndexedCount), number: .decimal)
                footerLeftLabel.stringValue = String(format: "⚡ %.2f ms · %@ encrypted items", ms, countFmt)
            } else {
                footerLeftLabel.stringValue = WebSuggestClient.isEnabled
                    ? "Fast Mac Search · Web Suggestions On"
                    : "Fast Mac Search · 100% Local Zero-Trust"
            }
        }

        let row = tableView.selectedRow
        guard row >= 0, row < rows.count else {
            footerRightLabel.stringValue = ""
            return
        }

        switch rows[row] {
        case .sectionHeader:
            footerRightLabel.stringValue = ""
        case .calculator(let calc):
            footerRightLabel.stringValue = calc.hex != nil
                ? "↵ Copy Answer   ⌘↵ Copy Hex"
                : "↵ Copy Answer"
        case .app:
            footerRightLabel.stringValue = "↵ Open Application   ⌘↵ Show in Finder"
        case .searchHit(let hit, _, _):
            if hit.is_llm_chat {
                footerRightLabel.stringValue = "↵ Read Conversation   ⌘↵ Reveal Transcript"
            } else {
                footerRightLabel.stringValue = "↵ Open   ⌘Y QuickLook   ⌘↵ Show in Finder"
            }
        case .web(let webKind, _):
            footerRightLabel.stringValue = webKind.opensInPanelPreview
                ? "↵ Preview in Panel   ⌘↵ Open in Browser"
                : "↵ Open in Browser"
        }
    }

    // MARK: - Keyboard Navigation & Execution

    private func moveSelection(delta: Int) {
        guard !rows.isEmpty else { return }
        var idx = tableView.selectedRow
        if idx < 0 {
            idx = delta >= 0 ? -1 : rows.count
        }
        var steps = 0
        while steps < rows.count {
            idx += (delta >= 0 ? 1 : -1)
            if idx < 0 || idx >= rows.count { break }
            if rows[idx].isSelectable {
                tableView.selectRowIndexes(IndexSet(integer: idx), byExtendingSelection: false)
                tableView.scrollRowToVisible(idx)
                updateSlidingSelectionPill(animated: true)
                updateFooterLabels()
                return
            }
            steps += 1
        }
    }

    private func triggerShortcutNumber(_ num: Int) -> Bool {
        for (rowIdx, item) in rows.enumerated() {
            let itemNum: Int?
            switch item {
            case .app(_, _, let n): itemNum = n
            case .searchHit(_, _, let n): itemNum = n
            case .web(_, let n): itemNum = n
            default: itemNum = nil
            }
            if itemNum == num {
                tableView.selectRowIndexes(IndexSet(integer: rowIdx), byExtendingSelection: false)
                updateSlidingSelectionPill(animated: false)
                executeRow(at: rowIdx, commandModified: false)
                return true
            }
        }
        return false
    }

    public func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        if commandSelector == #selector(NSResponder.moveDown(_:)) {
            moveSelection(delta: 1)
            return true
        }
        if commandSelector == #selector(NSResponder.moveUp(_:)) {
            moveSelection(delta: -1)
            return true
        }
        if commandSelector == #selector(NSResponder.insertTab(_:)) {
            if acceptInlineGhostIfAvailable() {
                return true
            }
        }
        // Right arrow at the very end of the query accepts inline ghost completion (just like Nanolight!)
        if commandSelector == #selector(NSResponder.moveRight(_:)) {
            let sel = textView.selectedRange()
            let len = (searchField.stringValue as NSString).length
            if sel.length == 0 && sel.location == len && acceptInlineGhostIfAvailable() {
                return true
            }
        }
        if commandSelector == #selector(NSResponder.insertNewline(_:)) {
            let flags = NSApp.currentEvent?.modifierFlags ?? []
            executeRow(at: tableView.selectedRow, commandModified: flags.contains(.command))
            return true
        }
        if commandSelector == #selector(NSResponder.cancelOperation(_:)) {
            if isShowingWebPreview {
                hideWebPreview()
            } else {
                onRequestDismiss?()
            }
            return true
        }
        return false
    }

    @objc private func openSelectedResult() {
        let clicked = tableView.clickedRow >= 0 ? tableView.clickedRow : tableView.selectedRow
        let flags = NSApp.currentEvent?.modifierFlags ?? []
        executeRow(at: clicked, commandModified: flags.contains(.command))
    }

    private func executeRow(at row: Int, commandModified: Bool) {
        guard row >= 0, row < rows.count else { return }
        switch rows[row] {
        case .sectionHeader:
            return

        case .calculator(let calc):
            let textToCopy = (commandModified && calc.hex != nil) ? calc.hex! : calc.formatted
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(textToCopy, forType: .string)
            flashFooterMessage("✓ Copied \(textToCopy) to Clipboard")

        case .app(let appHit, _, _):
            let appURL = URL(fileURLWithPath: appHit.app.path)
            if commandModified {
                NSWorkspace.shared.activateFileViewerSelecting([appURL])
                onRequestDismiss?()
            } else {
                appIndex.recordLaunch(of: appHit.app)
                NSWorkspace.shared.open(appURL)
                onRequestDismiss?()
            }

        case .searchHit(let hit, _, _):
            if commandModified {
                if FileManager.default.fileExists(atPath: hit.path) {
                    NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: hit.path)])
                    onRequestDismiss?()
                } else {
                    NSSound.beep()
                }
                return
            }

            if hit.is_llm_chat, let sid = hit.llm_session_id {
                let q = searchField.stringValue
                onRequestDismiss?()
                onOpenConversation?(sid, q)
                return
            }

            guard hit.volume_online else {
                NSSound.beep()
                return
            }
            let ctx = QueryContextPayload(activeAppBundleId: capturedAppBundleId)
            ipc.recordSelection(query: searchField.stringValue, selectedPath: hit.path, context: ctx)
            NSWorkspace.shared.open(URL(fileURLWithPath: hit.path))
            onRequestDismiss?()

        case .web(let webKind, _):
            guard let url = webKind.targetURL else { return }
            if !commandModified && webKind.opensInPanelPreview {
                showWebPreview(url: url)
            } else {
                NSWorkspace.shared.open(url)
                onRequestDismiss?()
            }
        }
    }

    // MARK: - Optional In-Panel WKWebView Preview (Nanolight-style)

    private func showWebPreview(url: URL) {
        activeWebPreviewURL = url
        let web: WKWebView
        if let existing = webPreviewView {
            web = existing
        } else {
            let cfg = WKWebViewConfiguration()
            cfg.websiteDataStore = .nonPersistent() // Ephemeral: zero persistent cookies/tracking!
            let created = WKWebView(frame: .zero, configuration: cfg)
            created.navigationDelegate = self
            created.uiDelegate = self
            created.customUserAgent = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15"
            vibrancyView.addSubview(created)
            webPreviewView = created
            web = created
        }
        web.isHidden = false
        web.load(URLRequest(url: url))
        resizePanelKeepingTopEdge(targetHeight: Self.webPreviewExpandedHeight)
        updateFooterLabels()
        view.window?.makeFirstResponder(web)
    }

    private func hideWebPreview(restoreFocus: Bool = true) {
        guard let web = webPreviewView, !web.isHidden else { return }
        web.stopLoading()
        web.loadHTMLString("", baseURL: nil)
        web.isHidden = true
        activeWebPreviewURL = nil

        var contentH: CGFloat = 6
        for item in rows {
            contentH += item.rowHeight + tableView.intercellSpacing.height
        }
        let desiredH = rows.isEmpty ? Self.collapsedHeight : (Self.collapsedHeight + contentH + Self.footerHeight + 4)
        resizePanelKeepingTopEdge(targetHeight: desiredH)
        updateFooterLabels()
        if restoreFocus {
            view.window?.makeFirstResponder(searchField)
        }
    }

    public func webView(
        _ webView: WKWebView,
        createWebViewWith configuration: WKWebViewConfiguration,
        for navigationAction: WKNavigationAction,
        windowFeatures: WKWindowFeatures
    ) -> WKWebView? {
        if navigationAction.targetFrame == nil, let reqURL = navigationAction.request.url {
            webView.load(URLRequest(url: reqURL))
        }
        return nil
    }

    // MARK: - QuickLook Preview (Files & AI Chats)

    public func toggleQuickLook() {
        let row = tableView.selectedRow
        guard row >= 0, row < rows.count else { return }

        switch rows[row] {
        case .searchHit(let hit, _, _):
            if hit.is_llm_chat, let sid = hit.llm_session_id {
                let q = searchField.stringValue
                onRequestDismiss?()
                onOpenConversation?(sid, q)
                return
            }
        case .app(let appHit, _, _):
            NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: appHit.app.path)])
            onRequestDismiss?()
            return
        default:
            return
        }

        guard let panel = QLPreviewPanel.shared() else { return }
        if QLPreviewPanel.sharedPreviewPanelExists() && panel.isVisible {
            panel.orderOut(nil)
        } else {
            panel.dataSource = self
            panel.delegate = self
            panel.makeKeyAndOrderFront(nil)
        }
    }

    // MARK: - NSTableView DataSource & Delegate

    public func numberOfRows(in tableView: NSTableView) -> Int {
        rows.count
    }

    public func tableView(_ tableView: NSTableView, heightOfRow row: Int) -> CGFloat {
        guard row >= 0, row < rows.count else { return 44 }
        return rows[row].rowHeight
    }

    public func tableView(_ tableView: NSTableView, shouldSelectRow row: Int) -> Bool {
        guard row >= 0, row < rows.count else { return false }
        return rows[row].isSelectable
    }

    public func tableViewSelectionDidChange(_ notification: Notification) {
        updateSlidingSelectionPill(animated: true)
        updateFooterLabels()
    }

    public func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard row >= 0, row < rows.count else { return nil }
        switch rows[row] {
        case .sectionHeader(let title):
            let id = NSUserInterfaceItemIdentifier("FMSSectionHeaderCell")
            let cell = (tableView.makeView(withIdentifier: id, owner: self) as? SectionHeaderCellView)
                ?? SectionHeaderCellView(identifier: id)
            cell.configure(title: title)
            return cell

        case .calculator(let calc):
            let id = NSUserInterfaceItemIdentifier("FMSCalculatorCell")
            let cell = (tableView.makeView(withIdentifier: id, owner: self) as? CalculatorRowView)
                ?? CalculatorRowView(identifier: id)
            cell.configure(with: calc)
            return cell

        case .app(let appHit, _, let shortcutNumber):
            let id = NSUserInterfaceItemIdentifier("FMSAppRowCell")
            let cell = (tableView.makeView(withIdentifier: id, owner: self) as? AppRowView)
                ?? AppRowView(identifier: id)
            cell.configure(with: appHit, icon: appIndex.icon(for: appHit.app.path), shortcutNumber: shortcutNumber)
            return cell

        case .searchHit(let hit, let matchedIndices, let shortcutNumber):
            let id = NSUserInterfaceItemIdentifier("FMSHitCell")
            let cell = (tableView.makeView(withIdentifier: id, owner: self) as? HitRowView)
                ?? HitRowView(identifier: id)
            cell.configure(with: hit, matchedIndices: matchedIndices, shortcutNumber: shortcutNumber)
            return cell

        case .web(let webKind, let shortcutNumber):
            let id = NSUserInterfaceItemIdentifier("FMSWebRowCell")
            let cell = (tableView.makeView(withIdentifier: id, owner: self) as? WebRowView)
                ?? WebRowView(identifier: id)
            cell.configure(with: webKind, shortcutNumber: shortcutNumber)
            return cell
        }
    }

    public nonisolated func numberOfPreviewItems(in panel: QLPreviewPanel!) -> Int {
        MainActor.assumeIsolated {
            let row = tableView.selectedRow
            guard row >= 0, row < rows.count else { return 0 }
            if case .searchHit(let hit, _, _) = rows[row], !hit.is_llm_chat {
                return 1
            }
            return 0
        }
    }

    public nonisolated func previewPanel(_ panel: QLPreviewPanel!, previewItemAt index: Int) -> (any QLPreviewItem)! {
        let selectedPath: String? = MainActor.assumeIsolated {
            let row = tableView.selectedRow
            guard row >= 0, row < rows.count else { return nil }
            if case .searchHit(let hit, _, _) = rows[row], !hit.is_llm_chat {
                return hit.path
            }
            return nil
        }
        guard let path = selectedPath else { return nil }
        return URL(fileURLWithPath: path) as NSURL
    }
}

// MARK: - Row Views (Section Header, Calculator Hero Card, App Row, File/AI Hit Row, Web Row)

@MainActor
private final class SectionHeaderCellView: NSTableCellView {
    private let label = NSTextField(labelWithString: "")

    init(identifier: NSUserInterfaceItemIdentifier) {
        super.init(frame: NSRect(x: 0, y: 0, width: 734, height: 25))
        self.identifier = identifier
        label.font = .systemFont(ofSize: 10.5, weight: .semibold)
        label.textColor = .tertiaryLabelColor
        label.frame = NSRect(x: 16, y: 4, width: 400, height: 15)
        addSubview(label)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    func configure(title: String) {
        label.stringValue = title
    }
}

@MainActor
private final class CalculatorRowView: NSTableCellView {
    private let exprLabel = NSTextField(labelWithString: "")
    private let subtitleLabel = NSTextField(labelWithString: "Calculator · Return copies answer")
    private let answerLabel = NSTextField(labelWithString: "")
    private let hexBadge = NSTextField(labelWithString: "")

    init(identifier: NSUserInterfaceItemIdentifier) {
        super.init(frame: NSRect(x: 0, y: 0, width: 734, height: 68))
        self.identifier = identifier

        exprLabel.font = .monospacedSystemFont(ofSize: 15, weight: .medium)
        exprLabel.textColor = .secondaryLabelColor
        exprLabel.lineBreakMode = .byTruncatingMiddle
        exprLabel.frame = NSRect(x: 20, y: 36, width: 320, height: 20)
        addSubview(exprLabel)

        subtitleLabel.font = .systemFont(ofSize: 11.5, weight: .regular)
        subtitleLabel.textColor = .tertiaryLabelColor
        subtitleLabel.frame = NSRect(x: 20, y: 14, width: 280, height: 16)
        addSubview(subtitleLabel)

        answerLabel.font = .monospacedDigitSystemFont(ofSize: 26, weight: .bold)
        answerLabel.textColor = .labelColor
        answerLabel.alignment = .right
        answerLabel.lineBreakMode = .byTruncatingHead
        answerLabel.frame = NSRect(x: 340, y: 26, width: 374, height: 32)
        addSubview(answerLabel)

        hexBadge.font = .monospacedSystemFont(ofSize: 11.5, weight: .medium)
        hexBadge.textColor = .systemCyan
        hexBadge.alignment = .right
        hexBadge.frame = NSRect(x: 480, y: 9, width: 234, height: 16)
        addSubview(hexBadge)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    func configure(with calc: CalcAnswerPayload) {
        exprLabel.stringValue = "\(calc.expression) ="
        answerLabel.stringValue = calc.formatted
        if let hex = calc.hex {
            hexBadge.isHidden = false
            hexBadge.stringValue = hex
            answerLabel.frame = NSRect(x: 340, y: 26, width: 374, height: 32)
        } else {
            hexBadge.isHidden = true
            answerLabel.frame = NSRect(x: 340, y: 18, width: 374, height: 32)
        }
    }
}

@MainActor
private final class AppRowView: NSTableCellView {
    private let iconView = NSImageView()
    private let nameLabel = NSTextField(labelWithString: "")
    private let folderLabel = NSTextField(labelWithString: "")
    private let badgeLabel = NSTextField(labelWithString: "")

    init(identifier: NSUserInterfaceItemIdentifier) {
        super.init(frame: NSRect(x: 0, y: 0, width: 734, height: 44))
        self.identifier = identifier

        iconView.frame = NSRect(x: 16, y: 7, width: 30, height: 30)
        addSubview(iconView)

        nameLabel.lineBreakMode = .byTruncatingTail
        nameLabel.frame = NSRect(x: 56, y: 13, width: 380, height: 19)
        addSubview(nameLabel)

        folderLabel.font = .systemFont(ofSize: 12, weight: .regular)
        folderLabel.textColor = .tertiaryLabelColor
        folderLabel.lineBreakMode = .byTruncatingTail
        folderLabel.frame = NSRect(x: 440, y: 13, width: 150, height: 17)
        addSubview(folderLabel)

        badgeLabel.font = .monospacedSystemFont(ofSize: 11, weight: .medium)
        badgeLabel.textColor = .secondaryLabelColor
        badgeLabel.alignment = .right
        badgeLabel.frame = NSRect(x: 595, y: 13, width: 120, height: 17)
        addSubview(badgeLabel)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    func configure(with hit: RankedAppHit, icon: NSImage, shortcutNumber: Int?) {
        iconView.image = icon

        let baseColor = hit.matchedCharIndices.isEmpty
            ? NSColor.labelColor
            : NSColor.labelColor.withAlphaComponent(0.76)
        nameLabel.attributedStringValue = LauncherMatcher.attributedHighlightedTitle(
            hit.app.name,
            matchedCharIndices: hit.matchedCharIndices,
            baseFont: .systemFont(ofSize: 14, weight: .medium),
            boldFont: .systemFont(ofSize: 14, weight: .bold),
            baseColor: baseColor,
            highlightColor: .labelColor
        )
        folderLabel.stringValue = hit.app.folder
        badgeLabel.stringValue = shortcutNumber != nil ? "Application · ⌥\(shortcutNumber!)" : "Application"
    }
}

@MainActor
private final class HitRowView: NSTableCellView {
    private static var iconCache: [String: NSImage] = [:]

    private let iconView = NSImageView()
    private let titleLabel = NSTextField(labelWithString: "")
    private let subtitleLabel = NSTextField(labelWithString: "")
    private let badgeLabel = NSTextField(labelWithString: "")

    init(identifier: NSUserInterfaceItemIdentifier) {
        super.init(frame: NSRect(x: 0, y: 0, width: 734, height: 50))
        self.identifier = identifier

        iconView.frame = NSRect(x: 16, y: 9, width: 30, height: 30)
        addSubview(iconView)

        titleLabel.lineBreakMode = .byTruncatingMiddle
        titleLabel.frame = NSRect(x: 56, y: 26, width: 495, height: 18)
        addSubview(titleLabel)

        subtitleLabel.font = .systemFont(ofSize: 11.5, weight: .regular)
        subtitleLabel.textColor = .secondaryLabelColor
        subtitleLabel.lineBreakMode = .byTruncatingTail
        subtitleLabel.frame = NSRect(x: 56, y: 7, width: 505, height: 16)
        addSubview(subtitleLabel)

        badgeLabel.font = .monospacedSystemFont(ofSize: 10.5, weight: .semibold)
        badgeLabel.alignment = .right
        badgeLabel.frame = NSRect(x: 568, y: 17, width: 147, height: 16)
        addSubview(badgeLabel)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    private static func cachedFileIcon(for hit: RankedHitPayload) -> NSImage {
        if hit.is_dir {
            if let cached = iconCache["__dir__"] { return cached }
            let img = NSWorkspace.shared.icon(for: .folder)
            iconCache["__dir__"] = img
            return img
        }
        let ext = (hit.filename as NSString).pathExtension.lowercased()
        let key = ext.isEmpty ? "__file__" : ext
        if let cached = iconCache[key] {
            return cached
        }
        let utType = UTType(filenameExtension: ext) ?? .data
        let img = NSWorkspace.shared.icon(for: utType)
        if iconCache.count > 256 {
            iconCache.removeAll(keepingCapacity: true)
        }
        iconCache[key] = img
        return img
    }

    func configure(with hit: RankedHitPayload, matchedIndices: [Int], shortcutNumber: Int?) {
        let baseColor = matchedIndices.isEmpty
            ? NSColor.labelColor
            : NSColor.labelColor.withAlphaComponent(0.78)
        titleLabel.attributedStringValue = LauncherMatcher.attributedHighlightedTitle(
            hit.filename,
            matchedCharIndices: matchedIndices,
            baseFont: .systemFont(ofSize: 13.5, weight: .medium),
            boldFont: .systemFont(ofSize: 13.5, weight: .bold),
            baseColor: baseColor,
            highlightColor: .labelColor
        )

        let shortcutSuffix = shortcutNumber != nil ? " · ⌥\(shortcutNumber!)" : ""

        if hit.is_llm_chat {
            let prov = ConversationViewerWindowController.displayProviderName(hit.llm_provider ?? hit.volume_name)
            let (sym, color) = ConversationViewerWindowController.providerSymbolAndColor(prov)
            let cfg = NSImage.SymbolConfiguration(pointSize: 19, weight: .semibold)
            iconView.image = NSImage(systemSymbolName: sym, accessibilityDescription: prov)?.withSymbolConfiguration(cfg)
            iconView.contentTintColor = color

            if let ws = hit.workspace, !ws.isEmpty {
                let shortWs = (ws as NSString).lastPathComponent
                subtitleLabel.stringValue = hit.snippet.isEmpty
                    ? "Workspace: \(shortWs)"
                    : "\(shortWs)  —  \(hit.snippet)"
            } else {
                subtitleLabel.stringValue = hit.snippet.isEmpty ? "Encrypted AI Conversation" : hit.snippet
            }

            badgeLabel.textColor = color
            badgeLabel.stringValue = "✦ \(prov)\(shortcutSuffix)"
            return
        }

        iconView.contentTintColor = nil
        iconView.image = Self.cachedFileIcon(for: hit)
        subtitleLabel.stringValue = hit.snippet.isEmpty ? hit.path : "\(hit.path)  —  \(hit.snippet)"

        if hit.volume_online {
            badgeLabel.textColor = .secondaryLabelColor
            badgeLabel.stringValue = "\(hit.matched_layers.joined(separator: "+"))\(shortcutSuffix)"
        } else {
            badgeLabel.textColor = .systemOrange
            badgeLabel.stringValue = "OFFLINE · \(hit.volume_name)"
        }
    }
}

@MainActor
private final class WebRowView: NSTableCellView {
    private let iconView = NSImageView()
    private let titleLabel = NSTextField(labelWithString: "")
    private let badgeLabel = NSTextField(labelWithString: "")

    init(identifier: NSUserInterfaceItemIdentifier) {
        super.init(frame: NSRect(x: 0, y: 0, width: 734, height: 40))
        self.identifier = identifier

        iconView.frame = NSRect(x: 19, y: 9, width: 22, height: 22)
        iconView.contentTintColor = .secondaryLabelColor
        addSubview(iconView)

        titleLabel.lineBreakMode = .byTruncatingTail
        titleLabel.frame = NSRect(x: 56, y: 11, width: 520, height: 18)
        addSubview(titleLabel)

        badgeLabel.font = .monospacedSystemFont(ofSize: 11, weight: .medium)
        badgeLabel.textColor = .secondaryLabelColor
        badgeLabel.alignment = .right
        badgeLabel.frame = NSRect(x: 585, y: 11, width: 130, height: 17)
        addSubview(badgeLabel)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    func configure(with kind: WebRowKind, shortcutNumber: Int?) {
        let cfg = NSImage.SymbolConfiguration(pointSize: 14, weight: .medium)
        iconView.image = NSImage(systemSymbolName: kind.symbolName, accessibilityDescription: kind.badge)?.withSymbolConfiguration(cfg)

        // Like Nanolight: draw the added completion words in bold!
        if case .googleSuggestion(let typed, let suggestion) = kind,
           suggestion.lowercased().hasPrefix(typed.lowercased()),
           suggestion.count > typed.count
        {
            let prefixStr = String(suggestion.prefix(typed.count))
            let addedStr = String(suggestion.dropFirst(typed.count))
            let attr = NSMutableAttributedString(
                string: prefixStr,
                attributes: [
                    .font: NSFont.systemFont(ofSize: 13.5, weight: .regular),
                    .foregroundColor: NSColor.secondaryLabelColor
                ]
            )
            attr.append(
                NSAttributedString(
                    string: addedStr,
                    attributes: [
                        .font: NSFont.systemFont(ofSize: 13.5, weight: .semibold),
                        .foregroundColor: NSColor.labelColor
                    ]
                )
            )
            titleLabel.attributedStringValue = attr
        } else {
            titleLabel.attributedStringValue = NSAttributedString(
                string: kind.title,
                attributes: [
                    .font: NSFont.systemFont(ofSize: 13.5, weight: .medium),
                    .foregroundColor: NSColor.labelColor
                ]
            )
        }

        let shortcutSuffix = shortcutNumber != nil ? " · ⌥\(shortcutNumber!)" : ""
        badgeLabel.stringValue = "\(kind.badge)\(shortcutSuffix)"
    }
}
