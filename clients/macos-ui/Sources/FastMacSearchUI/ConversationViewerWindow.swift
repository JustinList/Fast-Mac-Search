import AppKit

@MainActor
public final class ConversationViewerWindowController: NSWindowController {
    private let ipc: SearchdIPCClient
    private var currentSession: LlmSessionPayload?

    private let providerIcon = NSImageView()
    private let titleLabel = NSTextField(labelWithString: "")
    private let metaLabel = NSTextField(labelWithString: "")
    private let textView = NSTextView()
    private let copyThreadBtn = NSButton(title: "Copy Full Thread", target: nil, action: nil)
    private let copyResumeBtn = NSButton(title: "Copy Resume Cmd", target: nil, action: nil)
    private let openWorkspaceBtn = NSButton(title: "Open Workspace", target: nil, action: nil)
    private let revealSourceBtn = NSButton(title: "Reveal File", target: nil, action: nil)

    public init(ipc: SearchdIPCClient) {
        self.ipc = ipc

        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 780, height: 580),
            styleMask: [.titled, .closable, .resizable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Fast Mac Search — Encrypted AI Conversation"
        window.minSize = NSSize(width: 560, height: 400)
        window.center()
        window.isReleasedWhenClosed = false
        super.init(window: window)

        setupUI()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    private func setupUI() {
        guard let contentView = window?.contentView else { return }

        let w = contentView.bounds.width
        let h = contentView.bounds.height

        providerIcon.frame = NSRect(x: 20, y: h - 54, width: 32, height: 32)
        providerIcon.autoresizingMask = [.minYMargin]
        contentView.addSubview(providerIcon)

        titleLabel.font = .systemFont(ofSize: 16, weight: .semibold)
        titleLabel.textColor = .labelColor
        titleLabel.lineBreakMode = .byTruncatingTail
        titleLabel.frame = NSRect(x: 60, y: h - 40, width: w - 80, height: 22)
        titleLabel.autoresizingMask = [.width, .minYMargin]
        contentView.addSubview(titleLabel)

        metaLabel.font = .systemFont(ofSize: 12, weight: .regular)
        metaLabel.textColor = .secondaryLabelColor
        metaLabel.lineBreakMode = .byTruncatingMiddle
        metaLabel.frame = NSRect(x: 60, y: h - 60, width: w - 80, height: 16)
        metaLabel.autoresizingMask = [.width, .minYMargin]
        contentView.addSubview(metaLabel)

        let topDivider = NSBox(frame: NSRect(x: 0, y: h - 70, width: w, height: 1))
        topDivider.boxType = .separator
        topDivider.autoresizingMask = [.width, .minYMargin]
        contentView.addSubview(topDivider)

        let scroll = NSScrollView(frame: NSRect(x: 0, y: 52, width: w, height: h - 122))
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = true
        scroll.backgroundColor = .textBackgroundColor
        scroll.borderType = .noBorder
        scroll.autoresizingMask = [.width, .height]

        textView.frame = NSRect(x: 0, y: 0, width: w, height: h - 122)
        textView.isEditable = false
        textView.isSelectable = true
        textView.isRichText = true
        textView.importsGraphics = false
        textView.textContainerInset = NSSize(width: 22, height: 18)
        textView.autoresizingMask = [.width]
        textView.textContainer?.widthTracksTextView = true
        scroll.documentView = textView
        contentView.addSubview(scroll)

        let bottomDivider = NSBox(frame: NSRect(x: 0, y: 51, width: w, height: 1))
        bottomDivider.boxType = .separator
        bottomDivider.autoresizingMask = [.width, .maxYMargin]
        contentView.addSubview(bottomDivider)

        copyThreadBtn.bezelStyle = .rounded
        copyThreadBtn.target = self
        copyThreadBtn.action = #selector(handleCopyThread)
        copyThreadBtn.frame = NSRect(x: 18, y: 12, width: 142, height: 28)
        copyThreadBtn.autoresizingMask = [.maxXMargin, .maxYMargin]
        contentView.addSubview(copyThreadBtn)

        copyResumeBtn.bezelStyle = .rounded
        copyResumeBtn.target = self
        copyResumeBtn.action = #selector(handleCopyResumeCommand)
        copyResumeBtn.frame = NSRect(x: 166, y: 12, width: 148, height: 28)
        copyResumeBtn.autoresizingMask = [.maxXMargin, .maxYMargin]
        contentView.addSubview(copyResumeBtn)

        openWorkspaceBtn.bezelStyle = .rounded
        openWorkspaceBtn.target = self
        openWorkspaceBtn.action = #selector(handleOpenWorkspace)
        openWorkspaceBtn.frame = NSRect(x: 320, y: 12, width: 132, height: 28)
        openWorkspaceBtn.autoresizingMask = [.maxXMargin, .maxYMargin]
        contentView.addSubview(openWorkspaceBtn)

        revealSourceBtn.bezelStyle = .rounded
        revealSourceBtn.target = self
        revealSourceBtn.action = #selector(handleRevealSource)
        revealSourceBtn.frame = NSRect(x: 458, y: 12, width: 112, height: 28)
        revealSourceBtn.autoresizingMask = [.maxXMargin, .maxYMargin]
        contentView.addSubview(revealSourceBtn)

        let closeBtn = NSButton(title: "Done", target: self, action: #selector(handleClose))
        closeBtn.bezelStyle = .rounded
        closeBtn.keyEquivalent = "\u{1b}"
        closeBtn.frame = NSRect(x: w - 98, y: 12, width: 80, height: 28)
        closeBtn.autoresizingMask = [.minXMargin, .maxYMargin]
        contentView.addSubview(closeBtn)
    }

    public func presentConversation(sessionId: String, highlightQuery: String) {
        let client = self.ipc
        let cleanQuery = Self.stripScopeTag(highlightQuery)

        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            guard let session = try? client.getLlmConversation(sessionId: sessionId) else {
                DispatchQueue.main.async { NSSound.beep() }
                return
            }
            DispatchQueue.main.async { [weak self] in
                guard let self = self else { return }
                self.render(session: session, highlightQuery: cleanQuery)
                NSApp.activate(ignoringOtherApps: true)
                self.window?.makeKeyAndOrderFront(nil)
            }
        }
    }

    private func render(session: LlmSessionPayload, highlightQuery: String) {
        self.currentSession = session
        let displayProv = Self.displayProviderName(session.provider)
        window?.title = "[\(displayProv)] \(session.title)"

        let (symbolName, tintColor) = Self.providerSymbolAndColor(displayProv)
        let cfg = NSImage.SymbolConfiguration(pointSize: 22, weight: .semibold)
        providerIcon.image = NSImage(systemSymbolName: symbolName, accessibilityDescription: displayProv)?.withSymbolConfiguration(cfg)
        providerIcon.contentTintColor = tintColor

        titleLabel.stringValue = session.title

        var metaParts: [String] = [displayProv]
        if let m = session.model, !m.isEmpty, m.lowercased() != displayProv.lowercased() {
            metaParts.append(m)
        }
        if let ws = session.workspace, !ws.isEmpty {
            metaParts.append(ws)
        }
        metaParts.append("\(session.turns.count) turn\(session.turns.count == 1 ? "" : "s")")
        if session.updated_unix_secs > 0 {
            let d = Date(timeIntervalSince1970: TimeInterval(session.updated_unix_secs))
            let df = DateFormatter()
            df.dateStyle = .medium
            df.timeStyle = .short
            metaParts.append(df.string(from: d))
        }
        metaLabel.stringValue = metaParts.joined(separator: "  ·  ")

        let canResume = session.session_id.hasPrefix("claude:") || session.session_id.hasPrefix("codex:")
        copyResumeBtn.isHidden = !canResume
        copyResumeBtn.title = "Copy Resume Cmd"
        copyThreadBtn.title = "Copy Full Thread"

        if let ws = session.workspace, FileManager.default.fileExists(atPath: ws) {
            openWorkspaceBtn.isEnabled = true
        } else {
            openWorkspaceBtn.isEnabled = false
        }
        revealSourceBtn.isEnabled = FileManager.default.fileExists(atPath: session.source_path)

        // Build rich attributed conversation transcript
        let docAttr = NSMutableAttributedString()
        let paraHeader = NSMutableParagraphStyle()
        paraHeader.paragraphSpacingBefore = 12
        paraHeader.paragraphSpacing = 4

        let paraBody = NSMutableParagraphStyle()
        paraBody.lineSpacing = 3
        paraBody.paragraphSpacing = 14

        for (idx, turn) in session.turns.enumerated() {
            let isUser = (turn.role == "user")
            let roleTitle = isUser ? "YOU" : displayProv.uppercased()
            let roleColor: NSColor = isUser ? .systemBlue : tintColor

            var headerText = "\(roleTitle)  #\(idx + 1)"
            if let ts = turn.timestamp, !ts.isEmpty {
                headerText += "   ·   \(ts)"
            }
            headerText += "\n"

            let headerAttr = NSAttributedString(
                string: headerText,
                attributes: [
                    .font: NSFont.monospacedSystemFont(ofSize: 11.5, weight: .bold),
                    .foregroundColor: roleColor,
                    .paragraphStyle: paraHeader
                ]
            )
            docAttr.append(headerAttr)

            let bodyAttr = NSAttributedString(
                string: turn.text + "\n",
                attributes: [
                    .font: NSFont.systemFont(ofSize: 13.5, weight: .regular),
                    .foregroundColor: NSColor.labelColor,
                    .paragraphStyle: paraBody
                ]
            )
            docAttr.append(bodyAttr)
        }

        // Highlight query terms & jump directly to the first match!
        var firstHighlightRange: NSRange?
        let tokens = highlightQuery
            .components(separatedBy: CharacterSet.alphanumerics.inverted)
            .filter { $0.count >= 2 }

        if !tokens.isEmpty {
            let fullNS = docAttr.string as NSString
            let highlightBg = NSColor.systemYellow.withAlphaComponent(0.30)
            for tok in tokens {
                var searchRange = NSRange(location: 0, length: fullNS.length)
                while searchRange.location < fullNS.length {
                    let found = fullNS.range(of: tok, options: [.caseInsensitive], range: searchRange)
                    if found.location == NSNotFound { break }
                    docAttr.addAttribute(.backgroundColor, value: highlightBg, range: found)
                    if firstHighlightRange == nil {
                        firstHighlightRange = found
                    }
                    let nextLoc = found.location + max(found.length, 1)
                    searchRange = NSRange(location: nextLoc, length: fullNS.length - nextLoc)
                }
            }
        }

        textView.textStorage?.setAttributedString(docAttr)
        if let target = firstHighlightRange {
            textView.scrollRangeToVisible(target)
        } else {
            textView.scrollToBeginningOfDocument(nil)
        }
    }

    @objc private func handleCopyThread() {
        guard let s = currentSession else { return }
        let prov = Self.displayProviderName(s.provider)
        var md = "# [\(prov)] \(s.title)\n"
        if let ws = s.workspace { md += "- Workspace: `\(ws)`\n" }
        md += "\n---\n\n"
        for (i, t) in s.turns.enumerated() {
            let who = (t.role == "user") ? "User" : prov
            md += "### \(who) (#\(i + 1))\n\(t.text)\n\n"
        }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(md, forType: .string)
        copyThreadBtn.title = "✓ Copied!"
    }

    @objc private func handleCopyResumeCommand() {
        guard let s = currentSession else { return }
        let rawId: String
        let cliCmd: String
        if s.session_id.hasPrefix("claude:") {
            rawId = String(s.session_id.dropFirst("claude:".count)).components(separatedBy: ":").first ?? ""
            cliCmd = "claude --resume \(rawId)"
        } else if s.session_id.hasPrefix("codex:") {
            rawId = String(s.session_id.dropFirst("codex:".count))
            cliCmd = "codex resume \(rawId)"
        } else {
            return
        }
        let fullCmd: String
        if let ws = s.workspace, !ws.isEmpty {
            let escaped = ws.replacingOccurrences(of: "\"", with: "\\\"")
            fullCmd = "cd \"\(escaped)\" && \(cliCmd)"
        } else {
            fullCmd = cliCmd
        }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(fullCmd, forType: .string)
        copyResumeBtn.title = "✓ Copied Cmd!"
    }

    @objc private func handleOpenWorkspace() {
        guard let ws = currentSession?.workspace, FileManager.default.fileExists(atPath: ws) else { return }
        NSWorkspace.shared.open(URL(fileURLWithPath: ws))
    }

    @objc private func handleRevealSource() {
        guard let p = currentSession?.source_path, FileManager.default.fileExists(atPath: p) else { return }
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: p)])
    }

    @objc private func handleClose() {
        window?.close()
    }

    public static func displayProviderName(_ raw: String) -> String {
        switch raw.lowercased() {
        case "claude": return "Claude"
        case "codex": return "Codex"
        case "gemini": return "Gemini"
        case "chatgpt": return "ChatGPT"
        case "lmstudio", "lm studio": return "LM Studio"
        default: return raw
        }
    }

    public static func providerSymbolAndColor(_ display: String) -> (String, NSColor) {
        switch display {
        case "Claude": return ("sparkles", .systemOrange)
        case "Codex": return ("terminal.fill", .systemGreen)
        case "Gemini": return ("wand.and.stars", .systemBlue)
        case "ChatGPT": return ("bubble.left.and.bubble.right.fill", .systemTeal)
        default: return ("cpu.fill", .systemPurple)
        }
    }

    private static func stripScopeTag(_ q: String) -> String {
        let trimmed = q.trimmingCharacters(in: .whitespaces)
        guard trimmed.hasPrefix("@") else { return trimmed }
        let parts = trimmed.split(maxSplits: 1, whereSeparator: { $0.isWhitespace })
        return parts.count > 1 ? String(parts[1]) : ""
    }
}
