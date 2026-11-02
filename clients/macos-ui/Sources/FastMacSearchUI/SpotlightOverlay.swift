import AppKit
import Quartz
import UniformTypeIdentifiers
import WebKit

/// Non-activating floating Nanolight-style Liquid Glass overlay panel that joins all macOS Spaces,
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

    var subtitle: String {
        switch self {
        case .searchGoogle: return "google.com"
        case .googleSuggestion: return ""
        case .askGrok: return "grok.com"
        case .searchX: return "x.com · Latest"
        }
    }

    var footerActionTitle: String {
        switch self {
        case .searchGoogle, .googleSuggestion: return "Search Google"
        case .askGrok: return "Ask Grok"
        case .searchX: return "Search X"
        }
    }

    var symbolName: String {
        switch self {
        case .searchGoogle: return "globe"
        case .googleSuggestion: return "magnifyingglass"
        case .askGrok: return "sparkles"
        case .searchX: return "xmark"
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
    case calculator(CalcAnswerPayload, isCopied: Bool, shortcutNumber: Int?)
    case app(RankedAppHit, shortcutNumber: Int?)
    case searchHit(RankedHitPayload, matchedIndices: [Int], shortcutNumber: Int?)
    case web(WebRowKind, shortcutNumber: Int?)

    var isSelectable: Bool {
        if case .sectionHeader = self { return false }
        return true
    }

    var rowHeight: CGFloat {
        switch self {
        case .sectionHeader: return 28
        case .calculator: return 92
        case .app, .searchHit, .web: return 40
        }
    }

    var shortcutNumber: Int? {
        switch self {
        case .sectionHeader: return nil
        case .calculator(_, _, let n): return n
        case .app(_, let n): return n
        case .searchHit(_, _, let n): return n
        case .web(_, let n): return n
        }
    }

    var rowVerb: String {
        switch self {
        case .sectionHeader: return ""
        case .calculator: return "Copy"
        case .app: return "Open"
        case .searchHit(let hit, _, _): return hit.is_llm_chat ? "Read" : "Open"
        case .web: return "Go"
        }
    }

    var footerActionTitle: String {
        switch self {
        case .sectionHeader: return "Open"
        case .calculator: return "Copy Answer"
        case .app: return "Open Application"
        case .searchHit(let hit, _, _):
            if hit.is_llm_chat { return "Read Conversation" }
            return hit.is_dir ? "Open Folder" : "Open File"
        case .web(let kind, _): return kind.footerActionTitle
        }
    }
}

/// Reusable Nanolight-style translucent keycap badge (`↩`, `esc`, etc.).
@MainActor
private final class KeycapBadgeView: NSView {
    private let label = NSTextField(labelWithString: "")

    init(text: String) {
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 5
        layer?.borderWidth = 0.5

        label.stringValue = text
        label.font = .systemFont(ofSize: 11, weight: .medium)
        label.alignment = .center
        addSubview(label)
        updateText(text)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    func updateText(_ text: String) {
        label.stringValue = text
        label.sizeToFit()
        let w = max(22, ceil(label.frame.width) + 10)
        let h: CGFloat = 19
        frame.size = NSSize(width: w, height: h)
        label.frame = NSRect(x: 0, y: (h - label.frame.height) / 2 - 0.5, width: w, height: label.frame.height)
        refreshColors()
    }

    func refreshColors() {
        let isDark = effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
        layer?.backgroundColor = (isDark
            ? NSColor.white.withAlphaComponent(0.11)
            : NSColor.black.withAlphaComponent(0.07)).cgColor
        layer?.borderColor = (isDark
            ? NSColor.white.withAlphaComponent(0.16)
            : NSColor.black.withAlphaComponent(0.12)).cgColor
        label.textColor = .secondaryLabelColor
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        refreshColors()
    }
}

@MainActor
public final class SearchOverlayController: NSViewController, NSTextFieldDelegate, NSTableViewDataSource, NSTableViewDelegate, QLPreviewPanelDataSource, QLPreviewPanelDelegate, WKNavigationDelegate, WKUIDelegate {
    public static let barWidth: CGFloat = 680
    public static let collapsedHeight: CGFloat = 58
    public static let headerHeight: CGFloat = 58
    public static let footerHeight: CGFloat = 38
    public static let maxExpandedHeight: CGFloat = 548
    public static let webPreviewExpandedHeight: CGFloat = 560

    private let ipc: SearchdIPCClient
    private let appIndex = AppLauncherIndex()
    private let webSuggest = WebSuggestClient()

    public var onOpenDriveManager: (() -> Void)?
    public var onOpenConversation: ((String, String) -> Void)?
    public var onRequestDismiss: (() -> Void)?

    private var rows: [OverlayRowItem] = []
    private var latestCalc: CalcAnswerPayload?
    private var isCalcRecentlyCopied = false
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
    private var copiedResetWork: DispatchWorkItem?

    // Nanolight Glass Shell & Clean Header
    private let vibrancyView = NSVisualEffectView()
    private let tintOverlayView = NSView()
    private let searchIconView = NSImageView()
    private let searchField = NSTextField()
    private let ghostLabel = NSTextField(labelWithString: "")

    private let headerHairline = NSView()
    private let scrollView = NSScrollView()
    private let tableView = NSTableView()
    private let selectionPillView = NSView()

    // Nanolight Footer (`38pt`)
    private let footerHairline = NSView()
    private let footerBadgeTile = NSView()
    private let footerBadgeIcon = NSImageView()
    private let footerBrandLabel = NSTextField(labelWithString: "")
    private let scopeButtonAll = NSButton()
    private let scopeButtonFiles = NSButton()
    private let scopeButtonAI = NSButton()

    private let footerPrimaryActionLabel = NSTextField(labelWithString: "Open Application")
    private let footerReturnCap = KeycapBadgeView(text: "↩")
    private let footerPipeSeparator = NSTextField(labelWithString: "|")
    private let footerCloseButton = NSButton()
    private let footerEscCap = KeycapBadgeView(text: "esc")

    // Lazy In-Panel WKWebView Preview
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
        let h = Self.headerHeight + Self.footerHeight + 2

        vibrancyView.frame = NSRect(x: 0, y: 0, width: w, height: h)
        vibrancyView.autoresizingMask = [.width, .height]
        vibrancyView.material = .popover
        vibrancyView.blendingMode = .behindWindow
        vibrancyView.state = .active
        vibrancyView.wantsLayer = true
        vibrancyView.layer?.cornerRadius = 18
        vibrancyView.layer?.masksToBounds = true
        vibrancyView.layer?.borderWidth = 1.0

        tintOverlayView.wantsLayer = true
        vibrancyView.addSubview(tintOverlayView)

        // Clean Nanolight Header: SF Symbol `magnifyingglass` + full-width 21pt bare query input + ghost completion
        let magCfg = NSImage.SymbolConfiguration(pointSize: 18, weight: .medium)
        searchIconView.image = NSImage(systemSymbolName: "magnifyingglass", accessibilityDescription: "Search")?.withSymbolConfiguration(magCfg)
        searchIconView.contentTintColor = .secondaryLabelColor
        vibrancyView.addSubview(searchIconView)

        ghostLabel.font = .systemFont(ofSize: 21, weight: .regular)
        ghostLabel.textColor = .tertiaryLabelColor
        ghostLabel.lineBreakMode = .byClipping
        vibrancyView.addSubview(ghostLabel)

        searchField.placeholderString = "Search apps, files, AI chats (@claude), or do math…"
        searchField.font = .systemFont(ofSize: 21, weight: .regular)
        searchField.isBordered = false
        searchField.drawsBackground = false
        searchField.focusRingType = .none
        searchField.delegate = self
        vibrancyView.addSubview(searchField)

        headerHairline.wantsLayer = true
        vibrancyView.addSubview(headerHairline)

        // Sliding Nanolight frosted selection step inside NSTableView
        selectionPillView.wantsLayer = true
        selectionPillView.layer?.cornerRadius = 10
        selectionPillView.layer?.borderWidth = 0.5
        selectionPillView.layer?.zPosition = -1
        selectionPillView.isHidden = true

        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("NanolightCol"))
        column.width = w - 12
        tableView.addTableColumn(column)
        tableView.headerView = nil
        tableView.rowHeight = 40
        tableView.intercellSpacing = .zero
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
        vibrancyView.addSubview(scrollView)

        footerHairline.wantsLayer = true
        vibrancyView.addSubview(footerHairline)

        // Nanolight Footer Left: Mini app icon tile + brand/count telemetry + subtle All · Files · AI pills
        footerBadgeTile.wantsLayer = true
        footerBadgeTile.layer?.cornerRadius = 4.5
        footerBadgeTile.layer?.backgroundColor = NSColor.systemYellow.withAlphaComponent(0.18).cgColor
        vibrancyView.addSubview(footerBadgeTile)

        let boltCfg = NSImage.SymbolConfiguration(pointSize: 10, weight: .bold)
        footerBadgeIcon.image = NSImage(systemSymbolName: "bolt.fill", accessibilityDescription: nil)?.withSymbolConfiguration(boltCfg)
        footerBadgeIcon.contentTintColor = .systemYellow
        footerBadgeTile.addSubview(footerBadgeIcon)

        footerBrandLabel.font = .systemFont(ofSize: 12, weight: .medium)
        footerBrandLabel.textColor = .secondaryLabelColor
        footerBrandLabel.lineBreakMode = .byTruncatingTail
        vibrancyView.addSubview(footerBrandLabel)

        configureFooterScopePill(scopeButtonAll, title: "All", tag: 0)
        configureFooterScopePill(scopeButtonFiles, title: "Files", tag: 1)
        configureFooterScopePill(scopeButtonAI, title: "AI", tag: 2)
        vibrancyView.addSubview(scopeButtonAll)
        vibrancyView.addSubview(scopeButtonFiles)
        vibrancyView.addSubview(scopeButtonAI)

        // Nanolight Footer Right: `<Verb> [↩]  |  Close [esc]`
        footerPrimaryActionLabel.font = .systemFont(ofSize: 12, weight: .semibold)
        footerPrimaryActionLabel.textColor = .labelColor
        footerPrimaryActionLabel.alignment = .right
        vibrancyView.addSubview(footerPrimaryActionLabel)
        vibrancyView.addSubview(footerReturnCap)

        footerPipeSeparator.font = .systemFont(ofSize: 12, weight: .regular)
        footerPipeSeparator.textColor = .quaternaryLabelColor
        vibrancyView.addSubview(footerPipeSeparator)

        footerCloseButton.title = "Close"
        footerCloseButton.isBordered = false
        footerCloseButton.font = .systemFont(ofSize: 12, weight: .medium)
        footerCloseButton.contentTintColor = .secondaryLabelColor
        footerCloseButton.target = self
        footerCloseButton.action = #selector(didTapCloseButton)
        vibrancyView.addSubview(footerCloseButton)
        vibrancyView.addSubview(footerEscCap)

        self.view = vibrancyView
        refreshScopePillAppearance()
        installPanelKeyShortcutsMonitor()
        relayoutSubviews(forHeight: h)
    }

    private func configureFooterScopePill(_ btn: NSButton, title: String, tag: Int) {
        btn.title = title
        btn.tag = tag
        btn.isBordered = false
        btn.wantsLayer = true
        btn.layer?.cornerRadius = 5
        btn.font = .systemFont(ofSize: 11, weight: .medium)
        btn.target = self
        btn.action = #selector(didClickScopePill(_:))
        btn.toolTip = "Filter Scope (⌃\(tag + 1))"
    }

    @objc private func didClickScopePill(_ sender: NSButton) {
        selectScope(index: sender.tag)
        view.window?.makeFirstResponder(searchField)
    }

    private func refreshScopePillAppearance() {
        let isDark = vibrancyView.effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
        let activeBg = (isDark ? NSColor.white.withAlphaComponent(0.14) : NSColor.black.withAlphaComponent(0.09)).cgColor
        for btn in [scopeButtonAll, scopeButtonFiles, scopeButtonAI] {
            let isSelected = (btn.tag == selectedScopeIndex)
            btn.layer?.backgroundColor = isSelected ? activeBg : NSColor.clear.cgColor
            btn.contentTintColor = isSelected ? .labelColor : .tertiaryLabelColor
        }
    }

    private func installPanelKeyShortcutsMonitor() {
        keyDownMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self = self, self.view.window?.isKeyWindow == true else { return event }
            let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)

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

            // ⌘1 – ⌘9 immediately runs row #1–#9 (exact Nanolight shortcut!)
            if flags == .command, let chars = event.charactersIgnoringModifiers {
                if let digit = Int(chars), (1...9).contains(digit) {
                    if self.triggerShortcutNumber(digit) {
                        return nil
                    }
                }
                switch chars.lowercased() {
                case "y":
                    self.toggleQuickLook()
                    return nil
                case ",":
                    self.didTapDrivesButton()
                    return nil
                default:
                    break
                }
            }

            // ⌃1 / ⌃2 / ⌃3 switches All | Files | AI scope (and ⌃N / ⌃P moves rows)
            if flags == .control, let chars = event.charactersIgnoringModifiers?.lowercased() {
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
                case "n":
                    self.moveSelection(delta: 1)
                    return nil
                case "p":
                    self.moveSelection(delta: -1)
                    return nil
                default:
                    break
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
        let isDark = vibrancyView.effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua

        vibrancyView.layer?.borderColor = (isDark
            ? NSColor.white.withAlphaComponent(0.13)
            : NSColor.black.withAlphaComponent(0.10)).cgColor
        tintOverlayView.frame = NSRect(x: 0, y: 0, width: w, height: h)
        tintOverlayView.layer?.backgroundColor = (isDark
            ? NSColor(calibratedWhite: 0.09, alpha: 0.44)
            : NSColor.white.withAlphaComponent(0.42)).cgColor

        let hairColor = (isDark
            ? NSColor.white.withAlphaComponent(0.10)
            : NSColor.black.withAlphaComponent(0.08)).cgColor
        headerHairline.layer?.backgroundColor = hairColor
        footerHairline.layer?.backgroundColor = hairColor

        selectionPillView.layer?.backgroundColor = (isDark
            ? NSColor.white.withAlphaComponent(0.14)
            : NSColor.black.withAlphaComponent(0.08)).cgColor
        selectionPillView.layer?.borderColor = (isDark
            ? NSColor.white.withAlphaComponent(0.15)
            : NSColor.black.withAlphaComponent(0.07)).cgColor

        // Header (`58pt` from top)
        let headerTopY = h - Self.headerHeight
        searchIconView.frame = NSRect(x: 20, y: headerTopY + 18, width: 22, height: 22)
        ghostLabel.frame = NSRect(x: 50, y: headerTopY + 15, width: w - 70, height: 28)
        searchField.frame = NSRect(x: 50, y: headerTopY + 15, width: w - 70, height: 28)

        let hasBody = h > Self.headerHeight + 8
        headerHairline.isHidden = !hasBody
        footerHairline.isHidden = !hasBody
        footerBadgeTile.isHidden = !hasBody
        footerBrandLabel.isHidden = !hasBody
        scopeButtonAll.isHidden = !hasBody
        scopeButtonFiles.isHidden = !hasBody
        scopeButtonAI.isHidden = !hasBody
        footerPrimaryActionLabel.isHidden = !hasBody
        footerReturnCap.isHidden = !hasBody
        footerPipeSeparator.isHidden = !hasBody
        footerCloseButton.isHidden = !hasBody
        footerEscCap.isHidden = !hasBody

        if !hasBody {
            scrollView.isHidden = true
            webPreviewView?.isHidden = true
            return
        }

        headerHairline.frame = NSRect(x: 0, y: headerTopY, width: w, height: 1)
        footerHairline.frame = NSRect(x: 0, y: Self.footerHeight, width: w, height: 1)

        // Footer Left (`y = 0..38`)
        footerBadgeTile.frame = NSRect(x: 16, y: 10, width: 18, height: 18)
        footerBadgeIcon.frame = NSRect(x: 3, y: 3, width: 12, height: 12)
        footerBrandLabel.frame = NSRect(x: 40, y: 10, width: 218, height: 17)

        scopeButtonAll.frame = NSRect(x: 264, y: 9, width: 30, height: 20)
        scopeButtonFiles.frame = NSRect(x: 297, y: 9, width: 38, height: 20)
        scopeButtonAI.frame = NSRect(x: 338, y: 9, width: 28, height: 20)
        refreshScopePillAppearance()

        // Footer Right (`<Verb> [↩] | Close [esc]`)
        layoutFooterRightCluster(panelWidth: w)

        let bodyFrame = NSRect(
            x: 0,
            y: Self.footerHeight + 1,
            width: w,
            height: max(0, headerTopY - Self.footerHeight - 1)
        )

        if isShowingWebPreview {
            scrollView.isHidden = true
            webPreviewView?.frame = bodyFrame
        } else {
            scrollView.isHidden = false
            scrollView.frame = bodyFrame
            if let col = tableView.tableColumns.first {
                col.width = w - 12
            }
        }
    }

    private func layoutFooterRightCluster(panelWidth w: CGFloat) {
        footerEscCap.refreshColors()
        footerReturnCap.refreshColors()

        var rightX = w - 16
        let escSize = footerEscCap.frame.size
        rightX -= escSize.width
        footerEscCap.frame = NSRect(x: rightX, y: 9.5, width: escSize.width, height: escSize.height)

        footerCloseButton.sizeToFit()
        let closeW = ceil(footerCloseButton.frame.width)
        rightX -= (closeW + 4)
        footerCloseButton.frame = NSRect(x: rightX, y: 9, width: closeW, height: 20)

        rightX -= 14
        footerPipeSeparator.frame = NSRect(x: rightX, y: 10, width: 10, height: 17)

        let retSize = footerReturnCap.frame.size
        rightX -= (retSize.width + 4)
        footerReturnCap.frame = NSRect(x: rightX, y: 9.5, width: retSize.width, height: retSize.height)

        footerPrimaryActionLabel.sizeToFit()
        let verbW = min(190, ceil(footerPrimaryActionLabel.frame.width) + 4)
        rightX -= (verbW + 6)
        footerPrimaryActionLabel.frame = NSRect(x: rightX, y: 10, width: verbW, height: 17)
    }

    private func resizePanelKeepingTopEdge(targetHeight: CGFloat) {
        guard let win = view.window else { return }
        let maxH = isShowingWebPreview ? Self.webPreviewExpandedHeight : Self.maxExpandedHeight
        let clamped = max(Self.collapsedHeight, min(maxH, targetHeight))
        let oldFrame = win.frame
        if abs(oldFrame.height - clamped) < 0.5 {
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
        hideWebPreview(restoreFocus: false)
        refreshScopePillAppearance()
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
            searchField.placeholderString = "Search encrypted files, paths, or document contents…"
        case 2:
            searchField.placeholderString = "Search Claude, Codex, Gemini & ChatGPT conversations…"
        default:
            searchField.placeholderString = "Search apps, files, AI chats (@claude), or do math…"
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
        isCalcRecentlyCopied = false
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
        //    - `All`: Show Nanolight's `Suggestions` (top 5 most-opened macOS apps)!
        //    - `Files`: Clean minimal bar + footer.
        //    - `AI`: Show recent encrypted AI conversations!
        if trimmed.isEmpty {
            latestCalc = nil
            latestWebSuggestions = []
            latestDaemonGhostSuffix = nil
            latestElapsedMs = nil

            if scopeWire == "all" {
                latestAppHits = appIndex.suggestedApps(limit: 5)
                latestDaemonHits = []
                rebuildUnifiedRows(forQuery: rawText, isEmptySuggestions: true, animateSelection: false)
                return
            } else if scopeWire == "files" {
                latestAppHits = []
                latestDaemonHits = []
                rows = []
                updateGhostDisplay(rawInput: "", suffix: nil)
                selectionPillView.isHidden = true
                tableView.reloadData()
                updateFooterLabels()
                resizePanelKeepingTopEdge(targetHeight: Self.headerHeight + Self.footerHeight + 2)
                return
            }
        }

        // 2. Synchronous <0.05ms Nanolight Reflex (Calculator + macOS Applications + Cached Web Stand-Ins)
        if scopeWire == "all" && !hasProviderTag {
            latestCalc = MathCalculator.evaluate(trimmed)
            latestAppHits = appIndex.rankApps(query: trimmed, limit: 5)
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

        // Pure math expression fast-path (<0.02ms): Calculator hero card + any matching Apps
        if scopeWire == "all" && latestCalc != nil {
            latestDaemonHits = []
            latestDaemonGhostSuffix = nil
            latestElapsedMs = 0.01
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
        let fetchLimit = (scopeWire == "all") ? 7 : 11

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

                    self.latestElapsedMs = Double(resp.total_elapsed_us) / 1000.0
                    self.latestIndexedCount = resp.total_indexed_paths

                    self.rebuildUnifiedRows(
                        forQuery: rawText,
                        isEmptySuggestions: false,
                        animateSelection: false
                    )
                }
            } catch {
                DispatchQueue.main.async { [weak self] in
                    guard let self = self, self.querySeq == mySeq else { return }
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

        // Section 1: Calculator Hero Card
        if let calc = latestCalc {
            built.append(.sectionHeader("Calculator"))
            built.append(.calculator(calc, isCopied: isCalcRecentlyCopied, shortcutNumber: claimShortcut()))
        }

        // Section 2: Applications (or Empty-Query Frequent App Suggestions)
        if !latestAppHits.isEmpty {
            built.append(.sectionHeader(isEmptySuggestions ? "Suggestions" : "Applications"))
            for appHit in latestAppHits {
                built.append(.app(appHit, shortcutNumber: claimShortcut()))
            }
        }

        let aiHits = latestDaemonHits.filter { $0.is_llm_chat }
        let fileHits = latestDaemonHits.filter { !$0.is_llm_chat }

        // Section 3: AI Conversations
        if !aiHits.isEmpty {
            built.append(.sectionHeader(trimmed.isEmpty ? "Recent AI Conversations" : "AI Conversations"))
            for hit in aiHits {
                let indices = highlightNeedle.isEmpty
                    ? []
                    : (LauncherMatcher.score(query: highlightNeedle, name: hit.filename)?.matchedCharIndices ?? [])
                built.append(.searchHit(hit, matchedIndices: indices, shortcutNumber: claimShortcut()))
            }
        }

        // Section 4: Encrypted Files & Folders
        if !fileHits.isEmpty {
            built.append(.sectionHeader("Files"))
            for hit in fileHits {
                let indices = highlightNeedle.isEmpty
                    ? []
                    : (LauncherMatcher.score(query: highlightNeedle, name: hit.filename)?.matchedCharIndices ?? [])
                built.append(.searchHit(hit, matchedIndices: indices, shortcutNumber: claimShortcut()))
            }
        }

        // Section 5: Opt-In Privacy-Guarded Web Suggestions + Ask Grok / Search X
        if WebSuggestClient.isEnabled,
           currentScopeWireValue() == "all",
           !trimmed.isEmpty,
           !trimmed.hasPrefix("@"),
           latestCalc == nil
        {
            if !latestWebSuggestions.isEmpty {
                built.append(.sectionHeader("Google"))
                for sug in latestWebSuggestions.prefix(3) {
                    built.append(.web(.googleSuggestion(typed: trimmed, suggestion: sug), shortcutNumber: claimShortcut()))
                }
            }
            built.append(.sectionHeader("Ask & Search"))
            built.append(.web(.askGrok(query: trimmed), shortcutNumber: claimShortcut()))
            built.append(.web(.searchX(query: trimmed), shortcutNumber: claimShortcut()))
            built.append(.web(.searchGoogle(query: trimmed), shortcutNumber: claimShortcut()))
        }

        self.rows = built
        computeInlineGhostCompletion(rawInput: rawQuery)

        tableView.reloadData()

        var contentH: CGFloat = rows.isEmpty ? 0 : 12
        for item in rows {
            contentH += item.rowHeight
        }
        let desiredH = Self.headerHeight + Self.footerHeight + 2 + contentH
        resizePanelKeepingTopEdge(targetHeight: desiredH)

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
            refreshRowSelectionStates()
            updateSlidingSelectionPill(animated: animateSelection)
        } else {
            selectionPillView.isHidden = true
        }
        updateFooterLabels()
    }

    /// Safe `Array(Character)` prefix comparison so Unicode case-folding can never trap `String.Index`.
    private static func caseInsensitiveCompletionSuffix(input: String, candidate: String) -> String? {
        let inChars = Array(input)
        let candChars = Array(candidate)
        guard !inChars.isEmpty, candChars.count > inChars.count else { return nil }
        for idx in 0..<inChars.count {
            if inChars[idx].lowercased() != candChars[idx].lowercased() {
                return nil
            }
        }
        return String(candChars[inChars.count...])
    }

    private func computeInlineGhostCompletion(rawInput: String) {
        guard latestCalc == nil, !rawInput.isEmpty, rawInput == rawInput.trimmingCharacters(in: .newlines) else {
            updateGhostDisplay(rawInput: "", suffix: nil)
            return
        }

        if let topApp = latestAppHits.first?.app.name,
           let suffix = Self.caseInsensitiveCompletionSuffix(input: rawInput, candidate: topApp)
        {
            updateGhostDisplay(rawInput: rawInput, suffix: suffix)
            return
        }

        if let s1Suffix = latestDaemonGhostSuffix, !s1Suffix.isEmpty {
            updateGhostDisplay(rawInput: rawInput, suffix: s1Suffix)
            return
        }

        if WebSuggestClient.isEnabled,
           let topSug = latestWebSuggestions.first,
           let suffix = Self.caseInsensitiveCompletionSuffix(input: rawInput, candidate: topSug)
        {
            updateGhostDisplay(rawInput: rawInput, suffix: suffix)
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
        let font = searchField.font ?? .systemFont(ofSize: 21, weight: .regular)

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

    // MARK: - Smooth Sliding Selection Pill & Dynamic Nanolight Footer

    private func refreshRowSelectionStates() {
        let selected = tableView.selectedRow
        for r in 0..<rows.count {
            if let cell = tableView.view(atColumn: 0, row: r, makeIfNecessary: false) as? SelectableNanolightCell {
                cell.setRowSelected(r == selected)
            }
        }
    }

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
        let pillFrame = rawRect.insetBy(dx: 8, dy: 1)
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

    private func updateFooterLabels() {
        if isShowingWebPreview {
            footerBrandLabel.stringValue = activeWebPreviewURL?.host ?? "Web Preview"
            footerPrimaryActionLabel.stringValue = "Open in Browser"
            footerReturnCap.updateText("⌘↩")
            footerCloseButton.title = "Back"
            layoutFooterRightCluster(panelWidth: Self.barWidth)
            return
        }

        footerReturnCap.updateText("↩")
        footerCloseButton.title = "Close"

        if let ms = latestElapsedMs {
            footerBrandLabel.stringValue = String(format: "Fast Mac Search · %.2f ms", ms)
        } else {
            footerBrandLabel.stringValue = "Fast Mac Search · \(appIndex.appCount) apps"
        }

        let row = tableView.selectedRow
        if row >= 0, row < rows.count, rows[row].isSelectable {
            footerPrimaryActionLabel.stringValue = rows[row].footerActionTitle
        } else {
            footerPrimaryActionLabel.stringValue = "Manage Drives (⌘,)"
        }
        layoutFooterRightCluster(panelWidth: Self.barWidth)
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
                refreshRowSelectionStates()
                updateSlidingSelectionPill(animated: true)
                updateFooterLabels()
                return
            }
            steps += 1
        }
    }

    private func triggerShortcutNumber(_ num: Int) -> Bool {
        for (rowIdx, item) in rows.enumerated() where item.shortcutNumber == num {
            tableView.selectRowIndexes(IndexSet(integer: rowIdx), byExtendingSelection: false)
            refreshRowSelectionStates()
            updateSlidingSelectionPill(animated: false)
            executeRow(at: rowIdx, commandModified: false)
            return true
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
            } else if !searchField.stringValue.isEmpty {
                searchField.stringValue = ""
                runKeystrokeQuery("")
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

        case .calculator(let calc, _, let sc):
            let textToCopy = (commandModified && calc.hex != nil) ? (calc.hex ?? calc.formatted) : calc.formatted
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(textToCopy, forType: .string)

            copiedResetWork?.cancel()
            isCalcRecentlyCopied = true
            rows[row] = .calculator(calc, isCopied: true, shortcutNumber: sc)
            tableView.reloadData(forRowIndexes: IndexSet(integer: row), columnIndexes: IndexSet(integer: 0))
            refreshRowSelectionStates()

            let work = DispatchWorkItem { [weak self] in
                Task { @MainActor in
                    guard let self = self else { return }
                    self.isCalcRecentlyCopied = false
                    if row < self.rows.count, case .calculator(let cur, _, let curSc) = self.rows[row] {
                        self.rows[row] = .calculator(cur, isCopied: false, shortcutNumber: curSc)
                        self.tableView.reloadData(forRowIndexes: IndexSet(integer: row), columnIndexes: IndexSet(integer: 0))
                        self.refreshRowSelectionStates()
                    }
                }
            }
            copiedResetWork = work
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.4, execute: work)

        case .app(let appHit, _):
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

    // MARK: - Optional In-Panel WKWebView Preview

    private func showWebPreview(url: URL) {
        activeWebPreviewURL = url
        let web: WKWebView
        if let existing = webPreviewView {
            web = existing
        } else {
            let cfg = WKWebViewConfiguration()
            cfg.websiteDataStore = .nonPersistent()
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

        var contentH: CGFloat = rows.isEmpty ? 0 : 12
        for item in rows {
            contentH += item.rowHeight
        }
        let desiredH = Self.headerHeight + Self.footerHeight + 2 + contentH
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

    // MARK: - QuickLook Preview

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
        case .app(let appHit, _):
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
        guard row >= 0, row < rows.count else { return 40 }
        return rows[row].rowHeight
    }

    public func tableView(_ tableView: NSTableView, shouldSelectRow row: Int) -> Bool {
        guard row >= 0, row < rows.count else { return false }
        return rows[row].isSelectable
    }

    public func tableViewSelectionDidChange(_ notification: Notification) {
        refreshRowSelectionStates()
        updateSlidingSelectionPill(animated: true)
        updateFooterLabels()
    }

    public func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard row >= 0, row < rows.count else { return nil }
        let isSelected = (row == tableView.selectedRow)

        switch rows[row] {
        case .sectionHeader(let title):
            let id = NSUserInterfaceItemIdentifier("FMSSectionHeaderCell")
            let cell = (tableView.makeView(withIdentifier: id, owner: self) as? SectionHeaderCellView)
                ?? SectionHeaderCellView(identifier: id)
            cell.configure(title: title)
            return cell

        case .calculator(let calc, let isCopied, _):
            let id = NSUserInterfaceItemIdentifier("FMSCalculatorCell")
            let cell = (tableView.makeView(withIdentifier: id, owner: self) as? CalculatorHeroCardView)
                ?? CalculatorHeroCardView(identifier: id)
            cell.configure(with: calc, isCopied: isCopied)
            return cell

        case .app(let appHit, let shortcutNumber):
            let id = NSUserInterfaceItemIdentifier("FMSNanolightRow")
            let cell = (tableView.makeView(withIdentifier: id, owner: self) as? NanolightUnifiedRowView)
                ?? NanolightUnifiedRowView(identifier: id)
            cell.configureAsApp(
                hit: appHit,
                icon: appIndex.icon(for: appHit.app.path),
                shortcutNumber: shortcutNumber,
                isSelected: isSelected
            )
            return cell

        case .searchHit(let hit, let matchedIndices, let shortcutNumber):
            let id = NSUserInterfaceItemIdentifier("FMSNanolightRow")
            let cell = (tableView.makeView(withIdentifier: id, owner: self) as? NanolightUnifiedRowView)
                ?? NanolightUnifiedRowView(identifier: id)
            cell.configureAsSearchHit(
                hit: hit,
                matchedIndices: matchedIndices,
                shortcutNumber: shortcutNumber,
                isSelected: isSelected
            )
            return cell

        case .web(let webKind, let shortcutNumber):
            let id = NSUserInterfaceItemIdentifier("FMSNanolightRow")
            let cell = (tableView.makeView(withIdentifier: id, owner: self) as? NanolightUnifiedRowView)
                ?? NanolightUnifiedRowView(identifier: id)
            cell.configureAsWeb(
                kind: webKind,
                shortcutNumber: shortcutNumber,
                isSelected: isSelected
            )
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

// MARK: - Nanolight Cell Views

@MainActor
private protocol SelectableNanolightCell: AnyObject {
    func setRowSelected(_ selected: Bool)
}

@MainActor
private final class SectionHeaderCellView: NSTableCellView {
    private let label = NSTextField(labelWithString: "")

    init(identifier: NSUserInterfaceItemIdentifier) {
        super.init(frame: NSRect(x: 0, y: 0, width: 668, height: 28))
        self.identifier = identifier
        label.font = .systemFont(ofSize: 11.5, weight: .semibold)
        label.textColor = .secondaryLabelColor.withAlphaComponent(0.85)
        label.frame = NSRect(x: 18, y: 4, width: 400, height: 16)
        addSubview(label)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    func configure(title: String) {
        label.stringValue = title
    }
}

/// Nanolight / Raycast two-column Calculator Hero Card (`92pt` tall):
/// `[ Expression ]  ──( → )──  [ 3,072 / Result · 0xC00 ]`
@MainActor
private final class CalculatorHeroCardView: NSTableCellView {
    private let exprLabel = NSTextField(labelWithString: "")
    private let exprChip = NSView()
    private let exprChipLabel = NSTextField(labelWithString: "Expression")

    private let arrowCircle = NSView()
    private let arrowIcon = NSImageView()

    private let answerLabel = NSTextField(labelWithString: "")
    private let resultChip = NSView()
    private let resultChipLabel = NSTextField(labelWithString: "Result")

    init(identifier: NSUserInterfaceItemIdentifier) {
        super.init(frame: NSRect(x: 0, y: 0, width: 668, height: 92))
        self.identifier = identifier

        let halfW: CGFloat = (668 - 40 - 32 - 24) / 2

        exprLabel.font = .monospacedDigitSystemFont(ofSize: 20, weight: .medium)
        exprLabel.textColor = .labelColor.withAlphaComponent(0.85)
        exprLabel.alignment = .center
        exprLabel.lineBreakMode = .byTruncatingMiddle
        exprLabel.frame = NSRect(x: 20, y: 44, width: halfW, height: 28)
        addSubview(exprLabel)

        exprChip.wantsLayer = true
        exprChip.layer?.cornerRadius = 6
        exprChip.layer?.backgroundColor = NSColor.white.withAlphaComponent(0.09).cgColor
        exprChipLabel.font = .systemFont(ofSize: 11, weight: .medium)
        exprChipLabel.textColor = .secondaryLabelColor
        exprChipLabel.alignment = .center
        exprChip.addSubview(exprChipLabel)
        addSubview(exprChip)
        exprChipLabel.sizeToFit()
        let ecW = ceil(exprChipLabel.frame.width) + 16
        exprChip.frame = NSRect(x: 20 + (halfW - ecW) / 2, y: 16, width: ecW, height: 20)
        exprChipLabel.frame = NSRect(x: 0, y: 2.5, width: ecW, height: 15)

        arrowCircle.wantsLayer = true
        arrowCircle.layer?.cornerRadius = 16
        arrowCircle.layer?.backgroundColor = NSColor.white.withAlphaComponent(0.09).cgColor
        arrowCircle.frame = NSRect(x: 20 + halfW + 12, y: 30, width: 32, height: 32)
        let arrowCfg = NSImage.SymbolConfiguration(pointSize: 13, weight: .semibold)
        arrowIcon.image = NSImage(systemSymbolName: "arrow.right", accessibilityDescription: nil)?.withSymbolConfiguration(arrowCfg)
        arrowIcon.contentTintColor = .secondaryLabelColor
        arrowIcon.frame = NSRect(x: 8, y: 8, width: 16, height: 16)
        arrowCircle.addSubview(arrowIcon)
        addSubview(arrowCircle)

        let rightX = 20 + halfW + 12 + 32 + 12
        answerLabel.font = .monospacedDigitSystemFont(ofSize: 28, weight: .semibold)
        answerLabel.textColor = .labelColor
        answerLabel.alignment = .center
        answerLabel.lineBreakMode = .byTruncatingHead
        answerLabel.frame = NSRect(x: rightX, y: 42, width: halfW, height: 34)
        addSubview(answerLabel)

        resultChip.wantsLayer = true
        resultChip.layer?.cornerRadius = 6
        resultChipLabel.font = .systemFont(ofSize: 11, weight: .medium)
        resultChipLabel.alignment = .center
        resultChip.addSubview(resultChipLabel)
        addSubview(resultChip)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    func configure(with calc: CalcAnswerPayload, isCopied: Bool) {
        let halfW: CGFloat = (668 - 40 - 32 - 24) / 2
        let rightX = 20 + halfW + 12 + 32 + 12

        exprLabel.stringValue = calc.expression
        answerLabel.stringValue = calc.formatted

        if isCopied {
            resultChipLabel.stringValue = "Copied"
            resultChipLabel.textColor = .systemGreen
            resultChip.layer?.backgroundColor = NSColor.systemGreen.withAlphaComponent(0.18).cgColor
        } else {
            if let hex = calc.hex {
                resultChipLabel.stringValue = "Result · \(hex)"
            } else {
                resultChipLabel.stringValue = "Result"
            }
            resultChipLabel.textColor = .secondaryLabelColor
            resultChip.layer?.backgroundColor = NSColor.white.withAlphaComponent(0.09).cgColor
        }

        resultChipLabel.sizeToFit()
        let rcW = ceil(resultChipLabel.frame.width) + 16
        resultChip.frame = NSRect(x: rightX + (halfW - rcW) / 2, y: 16, width: rcW, height: 20)
        resultChipLabel.frame = NSRect(x: 0, y: 2.5, width: rcW, height: 15)
    }
}

/// Single-line `40pt` Nanolight Row:
/// `[26x26 Icon / Tile]   BoldMatchedTitle  MutedInlineSubtitle           [Open ↩] or [⌘2]`
@MainActor
private final class NanolightUnifiedRowView: NSTableCellView, SelectableNanolightCell {
    private static var fileIconCache: [String: NSImage] = [:]

    private let tileBg = NSView()
    private let iconView = NSImageView()
    private let lineLabel = NSTextField(labelWithString: "")

    private let selectedVerbLabel = NSTextField(labelWithString: "Open")
    private let selectedKeycap = KeycapBadgeView(text: "↩")
    private let shortcutLabel = NSTextField(labelWithString: "")

    private var currentVerb: String = "Open"
    private var currentShortcutNumber: Int?

    init(identifier: NSUserInterfaceItemIdentifier) {
        super.init(frame: NSRect(x: 0, y: 0, width: 668, height: 40))
        self.identifier = identifier

        tileBg.wantsLayer = true
        tileBg.layer?.cornerRadius = 6
        tileBg.frame = NSRect(x: 18, y: 7, width: 26, height: 26)
        addSubview(tileBg)

        iconView.frame = NSRect(x: 18, y: 7, width: 26, height: 26)
        addSubview(iconView)

        lineLabel.lineBreakMode = .byTruncatingTail
        lineLabel.frame = NSRect(x: 54, y: 10, width: 515, height: 20)
        addSubview(lineLabel)

        selectedVerbLabel.font = .systemFont(ofSize: 12, weight: .medium)
        selectedVerbLabel.textColor = .secondaryLabelColor
        selectedVerbLabel.alignment = .right
        addSubview(selectedVerbLabel)
        addSubview(selectedKeycap)

        shortcutLabel.font = .monospacedDigitSystemFont(ofSize: 12, weight: .medium)
        shortcutLabel.textColor = .secondaryLabelColor.withAlphaComponent(0.55)
        shortcutLabel.alignment = .right
        shortcutLabel.frame = NSRect(x: 610, y: 11, width: 42, height: 17)
        addSubview(shortcutLabel)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    private static func cachedFileIcon(for hit: RankedHitPayload) -> NSImage {
        if hit.is_dir {
            if let cached = fileIconCache["__dir__"] { return cached }
            let img = NSWorkspace.shared.icon(for: .folder)
            fileIconCache["__dir__"] = img
            return img
        }
        let ext = (hit.filename as NSString).pathExtension.lowercased()
        let key = ext.isEmpty ? "__file__" : ext
        if let cached = fileIconCache[key] { return cached }
        let utType = UTType(filenameExtension: ext) ?? .data
        let img = NSWorkspace.shared.icon(for: utType)
        if fileIconCache.count > 256 {
            fileIconCache.removeAll(keepingCapacity: true)
        }
        fileIconCache[key] = img
        return img
    }

    private func buildInlineLine(
        title: String,
        matchedIndices: [Int],
        subtitle: String,
        subtitleColor: NSColor = .secondaryLabelColor.withAlphaComponent(0.78)
    ) -> NSAttributedString {
        let baseColor = matchedIndices.isEmpty
            ? NSColor.labelColor
            : NSColor.labelColor.withAlphaComponent(0.82)
        let mutable = NSMutableAttributedString(
            attributedString: LauncherMatcher.attributedHighlightedTitle(
                title,
                matchedCharIndices: matchedIndices,
                baseFont: .systemFont(ofSize: 13.5, weight: .medium),
                boldFont: .systemFont(ofSize: 13.5, weight: .bold),
                baseColor: baseColor,
                highlightColor: .labelColor
            )
        )
        if !subtitle.isEmpty {
            mutable.append(
                NSAttributedString(
                    string: "   \(subtitle)",
                    attributes: [
                        .font: NSFont.systemFont(ofSize: 12, weight: .regular),
                        .foregroundColor: subtitleColor
                    ]
                )
            )
        }
        return mutable
    }

    func configureAsApp(hit: RankedAppHit, icon: NSImage, shortcutNumber: Int?, isSelected: Bool) {
        tileBg.isHidden = true
        iconView.frame = NSRect(x: 18, y: 7, width: 26, height: 26)
        iconView.contentTintColor = nil
        iconView.image = icon

        lineLabel.attributedStringValue = buildInlineLine(
            title: hit.app.name,
            matchedIndices: hit.matchedCharIndices,
            subtitle: hit.app.folder
        )
        currentVerb = "Open"
        currentShortcutNumber = shortcutNumber
        setRowSelected(isSelected)
    }

    func configureAsSearchHit(hit: RankedHitPayload, matchedIndices: [Int], shortcutNumber: Int?, isSelected: Bool) {
        if hit.is_llm_chat {
            let prov = ConversationViewerWindowController.displayProviderName(hit.llm_provider ?? hit.volume_name)
            let (sym, color) = ConversationViewerWindowController.providerSymbolAndColor(prov)
            tileBg.isHidden = false
            tileBg.layer?.backgroundColor = color.withAlphaComponent(0.16).cgColor

            let cfg = NSImage.SymbolConfiguration(pointSize: 13, weight: .semibold)
            iconView.frame = NSRect(x: 22, y: 11, width: 18, height: 18)
            iconView.image = NSImage(systemSymbolName: sym, accessibilityDescription: prov)?.withSymbolConfiguration(cfg)
            iconView.contentTintColor = color

            let sub: String
            if !hit.snippet.isEmpty {
                sub = "✦ \(prov) · \(hit.snippet)"
            } else if let ws = hit.workspace, !ws.isEmpty {
                sub = "✦ \(prov) · \((ws as NSString).lastPathComponent)"
            } else {
                sub = "✦ \(prov)"
            }
            lineLabel.attributedStringValue = buildInlineLine(
                title: hit.filename,
                matchedIndices: matchedIndices,
                subtitle: sub,
                subtitleColor: color.withAlphaComponent(0.9)
            )
            currentVerb = "Read"
        } else {
            tileBg.isHidden = true
            iconView.frame = NSRect(x: 18, y: 7, width: 26, height: 26)
            iconView.contentTintColor = nil
            iconView.image = Self.cachedFileIcon(for: hit)

            let parentDir = ((hit.path as NSString).deletingLastPathComponent as NSString).abbreviatingWithTildeInPath
            let sub = hit.volume_online
                ? parentDir
                : "OFFLINE · Connect \(hit.volume_name)"
            let subColor: NSColor = hit.volume_online
                ? .secondaryLabelColor.withAlphaComponent(0.75)
                : .systemOrange
            lineLabel.attributedStringValue = buildInlineLine(
                title: hit.filename,
                matchedIndices: matchedIndices,
                subtitle: sub,
                subtitleColor: subColor
            )
            currentVerb = "Open"
        }

        currentShortcutNumber = shortcutNumber
        setRowSelected(isSelected)
    }

    func configureAsWeb(kind: WebRowKind, shortcutNumber: Int?, isSelected: Bool) {
        let isDark = effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
        tileBg.isHidden = false
        tileBg.layer?.backgroundColor = (isDark
            ? NSColor.white.withAlphaComponent(0.09)
            : NSColor.black.withAlphaComponent(0.06)).cgColor

        let cfg = NSImage.SymbolConfiguration(pointSize: 12.5, weight: .medium)
        iconView.frame = NSRect(x: 22, y: 11, width: 18, height: 18)
        iconView.image = NSImage(systemSymbolName: kind.symbolName, accessibilityDescription: nil)?.withSymbolConfiguration(cfg)
        iconView.contentTintColor = .secondaryLabelColor

        if case .googleSuggestion(let typed, let suggestion) = kind {
            let typedChars = Array(typed)
            let sugChars = Array(suggestion)
            var prefixMatch = sugChars.count > typedChars.count
            if prefixMatch {
                for idx in 0..<typedChars.count where typedChars[idx].lowercased() != sugChars[idx].lowercased() {
                    prefixMatch = false
                    break
                }
            }
            if prefixMatch {
                let prefixStr = String(sugChars[0..<typedChars.count])
                let addedStr = String(sugChars[typedChars.count...])
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
                lineLabel.attributedStringValue = attr
            } else {
                lineLabel.attributedStringValue = buildInlineLine(title: suggestion, matchedIndices: [], subtitle: "Google")
            }
        } else {
            lineLabel.attributedStringValue = buildInlineLine(
                title: kind.title,
                matchedIndices: [],
                subtitle: kind.subtitle
            )
        }

        currentVerb = "Go"
        currentShortcutNumber = shortcutNumber
        setRowSelected(isSelected)
    }

    func setRowSelected(_ selected: Bool) {
        if selected {
            shortcutLabel.isHidden = true
            selectedVerbLabel.isHidden = false
            selectedKeycap.isHidden = false
            selectedVerbLabel.stringValue = currentVerb
            selectedVerbLabel.sizeToFit()
            selectedKeycap.refreshColors()

            let capSize = selectedKeycap.frame.size
            let capX = 652 - capSize.width
            selectedKeycap.frame = NSRect(x: capX, y: 10.5, width: capSize.width, height: capSize.height)
            let verbW = ceil(selectedVerbLabel.frame.width) + 4
            selectedVerbLabel.frame = NSRect(x: capX - verbW - 6, y: 11, width: verbW, height: 17)
            lineLabel.frame = NSRect(x: 54, y: 10, width: max(180, capX - verbW - 68), height: 20)
        } else {
            selectedVerbLabel.isHidden = true
            selectedKeycap.isHidden = true
            if let num = currentShortcutNumber {
                shortcutLabel.isHidden = false
                shortcutLabel.stringValue = "⌘\(num)"
                lineLabel.frame = NSRect(x: 54, y: 10, width: 548, height: 20)
            } else {
                shortcutLabel.isHidden = true
                lineLabel.frame = NSRect(x: 54, y: 10, width: 590, height: 20)
            }
        }
    }
}
