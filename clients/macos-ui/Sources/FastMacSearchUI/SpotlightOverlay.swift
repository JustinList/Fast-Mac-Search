import AppKit
import Quartz
import UniformTypeIdentifiers

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
        // Keep visible if QuickLook preview panel is currently key; otherwise auto-hide cleanly!
        if QLPreviewPanel.sharedPreviewPanelExists(), QLPreviewPanel.shared().isVisible {
            return
        }
        onResignKeyDismiss?()
    }
}

@MainActor
public final class SearchOverlayController: NSViewController, NSTextFieldDelegate, NSTableViewDataSource, NSTableViewDelegate, QLPreviewPanelDataSource, QLPreviewPanelDelegate {
    public static let barWidth: CGFloat = 720
    public static let collapsedHeight: CGFloat = 62
    public static let maxExpandedHeight: CGFloat = 468

    private let ipc: SearchdIPCClient
    public var onOpenDriveManager: (() -> Void)?
    public var onRequestDismiss: (() -> Void)?

    private var hits: [RankedHitPayload] = []
    private var currentGhost: GhostPredictionPayload?
    private var capturedAppBundleId: String?
    private let queryQueue = DispatchQueue(label: "dev.fastmacsearch.ui.query", qos: .userInteractive)
    private var querySeq: UInt64 = 0

    private let vibrancyView = NSVisualEffectView()
    private let iconLabel = NSTextField(labelWithString: "⌕")
    private let searchField = NSTextField()
    private let ghostLabel = NSTextField(labelWithString: "")
    private let latencyPill = NSTextField(labelWithString: "⌥Space")
    private let drivesBtn = NSButton()
    private let closeBtn = NSButton()
    private let divider = NSBox()
    private let scrollView = NSScrollView()
    private let tableView = NSTableView()

    public init(ipc: SearchdIPCClient) {
        self.ipc = ipc
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

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

        iconLabel.font = .systemFont(ofSize: 22, weight: .medium)
        iconLabel.textColor = .secondaryLabelColor
        vibrancyView.addSubview(iconLabel)

        ghostLabel.font = .systemFont(ofSize: 20, weight: .regular)
        ghostLabel.textColor = NSColor.tertiaryLabelColor
        ghostLabel.lineBreakMode = .byTruncatingTail
        vibrancyView.addSubview(ghostLabel)

        searchField.placeholderString = "Search files, content, or concepts…"
        searchField.font = .systemFont(ofSize: 20, weight: .regular)
        searchField.isBordered = false
        searchField.drawsBackground = false
        searchField.focusRingType = .none
        searchField.delegate = self
        vibrancyView.addSubview(searchField)

        latencyPill.font = .monospacedSystemFont(ofSize: 11, weight: .medium)
        latencyPill.textColor = .secondaryLabelColor
        latencyPill.alignment = .right
        vibrancyView.addSubview(latencyPill)

        // 1-click Drives & Folders picker button directly on the search bar
        drivesBtn.bezelStyle = .inline
        drivesBtn.isBordered = false
        drivesBtn.image = NSImage(systemSymbolName: "externaldrive.badge.plus", accessibilityDescription: "Manage Drives & Folders")
        drivesBtn.contentTintColor = .secondaryLabelColor
        drivesBtn.toolTip = "Add or Manage Indexed Drives & Folders"
        drivesBtn.target = self
        drivesBtn.action = #selector(didTapDrivesButton)
        vibrancyView.addSubview(drivesBtn)

        // 1-click Close button (in addition to Esc / ⌥Space / clicking outside)
        closeBtn.bezelStyle = .inline
        closeBtn.isBordered = false
        closeBtn.image = NSImage(systemSymbolName: "xmark.circle.fill", accessibilityDescription: "Hide Search Bar")
        closeBtn.contentTintColor = .tertiaryLabelColor
        closeBtn.toolTip = "Hide Search Bar (Esc or ⌥Space)"
        closeBtn.target = self
        closeBtn.action = #selector(didTapCloseButton)
        vibrancyView.addSubview(closeBtn)

        divider.boxType = .separator
        divider.isHidden = true
        vibrancyView.addSubview(divider)

        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("ResultCol"))
        column.width = w - 24
        tableView.addTableColumn(column)
        tableView.headerView = nil
        tableView.rowHeight = 50
        tableView.backgroundColor = .clear
        tableView.selectionHighlightStyle = .regular
        tableView.style = .inset
        tableView.dataSource = self
        tableView.delegate = self
        tableView.target = self
        tableView.doubleAction = #selector(openSelectedResult)

        scrollView.drawsBackground = false
        scrollView.hasVerticalScroller = true
        scrollView.documentView = tableView
        scrollView.isHidden = true
        vibrancyView.addSubview(scrollView)

        self.view = vibrancyView
        relayoutSubviews(forHeight: h)
    }

    private func relayoutSubviews(forHeight h: CGFloat) {
        let w = Self.barWidth
        let headerCenterY = h - (Self.collapsedHeight / 2.0)

        iconLabel.frame = NSRect(x: 18, y: headerCenterY - 14, width: 28, height: 28)
        ghostLabel.frame = NSRect(x: 48, y: headerCenterY - 14, width: w - 295, height: 28)
        searchField.frame = NSRect(x: 48, y: headerCenterY - 14, width: w - 295, height: 28)
        latencyPill.frame = NSRect(x: w - 242, y: headerCenterY - 8, width: 170, height: 18)
        drivesBtn.frame = NSRect(x: w - 64, y: headerCenterY - 12, width: 24, height: 24)
        closeBtn.frame = NSRect(x: w - 34, y: headerCenterY - 12, width: 22, height: 22)

        let showResults = h > Self.collapsedHeight + 4
        divider.isHidden = !showResults
        scrollView.isHidden = !showResults
        if showResults {
            divider.frame = NSRect(x: 0, y: h - Self.collapsedHeight, width: w, height: 1)
            scrollView.frame = NSRect(x: 0, y: 0, width: w, height: h - Self.collapsedHeight - 1)
        }
    }

    private func resizePanelKeepingTopEdge(targetHeight: CGFloat) {
        guard let win = view.window else { return }
        let clamped = max(Self.collapsedHeight, min(Self.maxExpandedHeight, targetHeight))
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

    public func prepareForPresentation(previousAppBundleId: String?) {
        self.capturedAppBundleId = previousAppBundleId
        let desiredHeight = hits.isEmpty
            ? Self.collapsedHeight
            : (Self.collapsedHeight + CGFloat(hits.count) * 52 + 12)
        resizePanelKeepingTopEdge(targetHeight: desiredHeight)
        self.view.window?.makeFirstResponder(searchField)
        searchField.selectText(nil)
    }

    @objc private func didTapDrivesButton() {
        onRequestDismiss?()
        onOpenDriveManager?()
    }

    @objc private func didTapCloseButton() {
        onRequestDismiss?()
    }

    public func controlTextDidChange(_ obj: Notification) {
        runKeystrokeQuery(searchField.stringValue)
    }

    private func runKeystrokeQuery(_ text: String) {
        querySeq &+= 1
        let mySeq = querySeq
        let trimmed = text.trimmingCharacters(in: .whitespaces)
        if trimmed.isEmpty {
            self.hits = []
            self.currentGhost = nil
            self.ghostLabel.stringValue = ""
            self.latencyPill.stringValue = "⌥Space · Esc to hide"
            self.tableView.reloadData()
            resizePanelKeepingTopEdge(targetHeight: Self.collapsedHeight)
            return
        }

        let ctx = QueryContextPayload(activeAppBundleId: capturedAppBundleId)
        let client = self.ipc

        queryQueue.async { [weak self] in
            do {
                let resp = try client.query(text, limit: 10, context: ctx)
                DispatchQueue.main.async { [weak self] in
                    guard let self = self, self.querySeq == mySeq else { return }

                    self.hits = resp.hits
                    self.currentGhost = resp.ghost_prediction
                    self.ghostLabel.stringValue = resp.ghost_prediction?.full_query ?? ""

                    let ms = Double(resp.total_elapsed_us) / 1000.0
                    if resp.total_indexed_paths == 0 {
                        self.latencyPill.stringValue = "Click drive icon ↗ to index"
                    } else {
                        self.latencyPill.stringValue = String(format: "⚡ %.2f ms · %@", ms, resp.intent)
                    }

                    self.tableView.reloadData()
                    let targetH = self.hits.isEmpty
                        ? Self.collapsedHeight
                        : (Self.collapsedHeight + CGFloat(self.hits.count) * 52 + 12)
                    self.resizePanelKeepingTopEdge(targetHeight: targetH)

                    if !self.hits.isEmpty {
                        self.tableView.selectRowIndexes(IndexSet(integer: 0), byExtendingSelection: false)
                    }
                }
            } catch {
                DispatchQueue.main.async { [weak self] in
                    guard let self = self, self.querySeq == mySeq else { return }
                    self.latencyPill.stringValue = "Starting engine…"
                    self.ghostLabel.stringValue = ""
                }
            }
        }
    }

    public func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        if commandSelector == #selector(NSResponder.moveDown(_:)) {
            let next = min(tableView.selectedRow + 1, hits.count - 1)
            if next >= 0 {
                tableView.selectRowIndexes(IndexSet(integer: next), byExtendingSelection: false)
                tableView.scrollRowToVisible(next)
            }
            return true
        }
        if commandSelector == #selector(NSResponder.moveUp(_:)) {
            let prev = max(tableView.selectedRow - 1, 0)
            if !hits.isEmpty {
                tableView.selectRowIndexes(IndexSet(integer: prev), byExtendingSelection: false)
                tableView.scrollRowToVisible(prev)
            }
            return true
        }
        if commandSelector == #selector(NSResponder.insertTab(_:)) {
            if let ghost = currentGhost {
                searchField.stringValue = ghost.full_query
                currentGhost = nil
                ghostLabel.stringValue = ""
                runKeystrokeQuery(searchField.stringValue)
                return true
            }
        }
        if commandSelector == #selector(NSResponder.insertNewline(_:)) {
            let flags = NSApp.currentEvent?.modifierFlags ?? []
            if flags.contains(.command) {
                revealSelectedInFinder()
            } else {
                openSelectedResult()
            }
            return true
        }
        if commandSelector == #selector(NSResponder.cancelOperation(_:)) {
            onRequestDismiss?()
            return true
        }
        return false
    }

    @objc private func openSelectedResult() {
        let row = tableView.selectedRow
        guard row >= 0 && row < hits.count else { return }
        let hit = hits[row]
        guard hit.volume_online else {
            NSSound.beep()
            return
        }
        let ctx = QueryContextPayload(activeAppBundleId: capturedAppBundleId)
        ipc.recordSelection(query: searchField.stringValue, selectedPath: hit.path, context: ctx)
        NSWorkspace.shared.open(URL(fileURLWithPath: hit.path))
        onRequestDismiss?()
    }

    private func revealSelectedInFinder() {
        let row = tableView.selectedRow
        guard row >= 0 && row < hits.count else { return }
        let hit = hits[row]
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: hit.path)])
        onRequestDismiss?()
    }

    public func toggleQuickLook() {
        guard let panel = QLPreviewPanel.shared() else { return }
        if QLPreviewPanel.sharedPreviewPanelExists() && panel.isVisible {
            panel.orderOut(nil)
        } else {
            panel.dataSource = self
            panel.delegate = self
            panel.makeKeyAndOrderFront(nil)
        }
    }

    public func numberOfRows(in tableView: NSTableView) -> Int {
        hits.count
    }

    public func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard row >= 0, row < hits.count else { return nil }
        let hit = hits[row]
        let cellId = NSUserInterfaceItemIdentifier("FMSHitCell")
        let cell = (tableView.makeView(withIdentifier: cellId, owner: self) as? HitRowView) ?? HitRowView(identifier: cellId)
        cell.configure(with: hit)
        return cell
    }

    public nonisolated func numberOfPreviewItems(in panel: QLPreviewPanel!) -> Int {
        MainActor.assumeIsolated {
            tableView.selectedRow >= 0 && tableView.selectedRow < hits.count ? 1 : 0
        }
    }

    public nonisolated func previewPanel(_ panel: QLPreviewPanel!, previewItemAt index: Int) -> (any QLPreviewItem)! {
        let selectedPath: String? = MainActor.assumeIsolated {
            let row = tableView.selectedRow
            guard row >= 0 && row < hits.count else { return nil }
            return hits[row].path
        }
        guard let path = selectedPath else { return nil }
        return URL(fileURLWithPath: path) as NSURL
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
        super.init(frame: NSRect(x: 0, y: 0, width: 690, height: 50))
        self.identifier = identifier

        iconView.frame = NSRect(x: 12, y: 9, width: 32, height: 32)
        addSubview(iconView)

        titleLabel.font = .systemFont(ofSize: 14, weight: .semibold)
        titleLabel.textColor = .labelColor
        titleLabel.lineBreakMode = .byTruncatingMiddle
        titleLabel.frame = NSRect(x: 54, y: 25, width: 450, height: 18)
        addSubview(titleLabel)

        subtitleLabel.font = .systemFont(ofSize: 11.5, weight: .regular)
        subtitleLabel.textColor = .secondaryLabelColor
        subtitleLabel.lineBreakMode = .byTruncatingMiddle
        subtitleLabel.frame = NSRect(x: 54, y: 7, width: 490, height: 16)
        addSubview(subtitleLabel)

        badgeLabel.font = .monospacedSystemFont(ofSize: 10.5, weight: .medium)
        badgeLabel.alignment = .right
        badgeLabel.frame = NSRect(x: 550, y: 17, width: 130, height: 16)
        addSubview(badgeLabel)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    private static func cachedIcon(for hit: RankedHitPayload) -> NSImage {
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

    func configure(with hit: RankedHitPayload) {
        iconView.image = Self.cachedIcon(for: hit)
        titleLabel.stringValue = hit.filename
        subtitleLabel.stringValue = hit.snippet.isEmpty ? hit.path : "\(hit.path)  —  \(hit.snippet)"

        if hit.volume_online {
            badgeLabel.textColor = .secondaryLabelColor
            badgeLabel.stringValue = hit.matched_layers.joined(separator: "+")
        } else {
            badgeLabel.textColor = .systemOrange
            badgeLabel.stringValue = "OFFLINE · \(hit.volume_name)"
        }
    }
}
