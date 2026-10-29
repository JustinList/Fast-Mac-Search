import AppKit

public struct DiscoveredDrive: Sendable {
    public let name: String
    public let mountPath: String
    public let isExternal: Bool
    public let totalBytes: Int64
    public let isOnline: Bool
    public let matchingShard: ShardStatusPayload?
}

@MainActor
public final class DriveManagerWindowController: NSWindowController, NSTableViewDataSource, NSTableViewDelegate {
    private let ipc: SearchdIPCClient
    private let onTriggerFolderPicker: () -> Void
    private let onRequestRefresh: () -> Void

    private var rows: [DiscoveredDrive] = []
    private let tableView = NSTableView()
    private let statusBanner = NSTextField(labelWithString: "Select any drive or folder to encrypt and index for instant search.")
    private let portableToggle = NSButton(checkboxWithTitle: "Store portable encrypted index (.fastindex) on external drives", target: nil, action: nil)

    public init(
        ipc: SearchdIPCClient,
        onTriggerFolderPicker: @escaping () -> Void,
        onRequestRefresh: @escaping () -> Void
    ) {
        self.ipc = ipc
        self.onTriggerFolderPicker = onTriggerFolderPicker
        self.onRequestRefresh = onRequestRefresh

        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 640, height: 420),
            styleMask: [.titled, .closable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Fast Mac Search — Drives & Encrypted Shards"
        window.center()
        window.isReleasedWhenClosed = false
        super.init(window: window)

        setupUI()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    private func setupUI() {
        guard let contentView = window?.contentView else { return }

        let headerTitle = NSTextField(labelWithString: "Drives & Indexed Locations")
        headerTitle.font = .systemFont(ofSize: 18, weight: .semibold)
        headerTitle.frame = NSRect(x: 20, y: 376, width: 400, height: 24)
        contentView.addSubview(headerTitle)

        statusBanner.font = .systemFont(ofSize: 12, weight: .regular)
        statusBanner.textColor = .secondaryLabelColor
        statusBanner.frame = NSRect(x: 20, y: 352, width: 600, height: 18)
        contentView.addSubview(statusBanner)

        let col = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("DriveCol"))
        col.width = 580
        tableView.addTableColumn(col)
        tableView.headerView = nil
        tableView.rowHeight = 64
        tableView.style = .inset
        tableView.selectionHighlightStyle = .none
        tableView.dataSource = self
        tableView.delegate = self

        let scroll = NSScrollView(frame: NSRect(x: 20, y: 74, width: 600, height: 268))
        scroll.hasVerticalScroller = true
        scroll.borderType = .bezelBorder
        scroll.documentView = tableView
        contentView.addSubview(scroll)

        portableToggle.frame = NSRect(x: 20, y: 44, width: 440, height: 20)
        portableToggle.font = .systemFont(ofSize: 12)
        portableToggle.state = .off
        contentView.addSubview(portableToggle)

        let addFolderBtn = NSButton(title: "Add Specific Folder…", target: self, action: #selector(handleAddFolder))
        addFolderBtn.bezelStyle = .rounded
        addFolderBtn.frame = NSRect(x: 20, y: 12, width: 170, height: 28)
        contentView.addSubview(addFolderBtn)

        let quickDocsBtn = NSButton(title: "Index ~/Documents", target: self, action: #selector(handleIndexDocuments))
        quickDocsBtn.bezelStyle = .rounded
        quickDocsBtn.frame = NSRect(x: 198, y: 12, width: 150, height: 28)
        contentView.addSubview(quickDocsBtn)

        let doneBtn = NSButton(title: "Done", target: self, action: #selector(handleClose))
        doneBtn.bezelStyle = .rounded
        doneBtn.keyEquivalent = "\r"
        doneBtn.frame = NSRect(x: 530, y: 12, width: 90, height: 28)
        contentView.addSubview(doneBtn)
    }

    public func presentAndRefresh(with status: EngineStatusPayload?) {
        updateFromStatus(status)
        NSApp.activate(ignoringOtherApps: true)
        window?.makeKeyAndOrderFront(nil)
    }

    public func updateFromStatus(_ status: EngineStatusPayload?) {
        self.rows = Self.discoverDrives(merging: status)
        if let st = status {
            if st.indexing.is_indexing {
                let scanned = NumberFormatter.localizedString(from: NSNumber(value: st.indexing.files_scanned), number: .decimal)
                statusBanner.textColor = .systemBlue
                statusBanner.stringValue = "⚡ \(st.indexing.phase) on \(st.indexing.active_target) — \(scanned) files scanned…"
            } else {
                let total = NumberFormatter.localizedString(from: NSNumber(value: st.total_paths), number: .decimal)
                statusBanner.textColor = .secondaryLabelColor
                statusBanner.stringValue = "✓ Zero-Trust AES-256-GCM Active · \(total) paths indexed across \(st.shard_count) drive shard(s)"
            }
        }
        tableView.reloadData()
    }

    public static func discoverDrives(merging status: EngineStatusPayload?) -> [DiscoveredDrive] {
        let keys: [URLResourceKey] = [
            .volumeNameKey,
            .volumeIsRemovableKey,
            .volumeIsEjectableKey,
            .volumeIsInternalKey,
            .volumeTotalCapacityKey,
            .volumeIsBrowsableKey
        ]
        let urls = FileManager.default.mountedVolumeURLs(
            includingResourceValuesForKeys: keys,
            options: [.skipHiddenVolumes]
        ) ?? []

        let shards = status?.shards ?? []
        var usedShardUuids = Set<String>()
        var list: [DiscoveredDrive] = []

        for url in urls {
            let path = url.path
            if path.hasPrefix("/System/Volumes/") || path == "/dev" || path == "/private/var/vm" {
                continue
            }
            let vals = try? url.resourceValues(forKeys: Set(keys))
            let name = vals?.volumeName ?? (path == "/" ? "Macintosh HD" : url.lastPathComponent)
            let isInternal = vals?.volumeIsInternal ?? (path == "/")
            let isRemovable = (vals?.volumeIsRemovable ?? false) || (vals?.volumeIsEjectable ?? false)
            let isExternal = !isInternal || isRemovable || path.hasPrefix("/Volumes/")
            let cap = Int64(vals?.volumeTotalCapacity ?? 0)

            let matched = shards.first { $0.mount_point == path }
            if let m = matched {
                usedShardUuids.insert(m.volume_uuid)
            }

            list.append(DiscoveredDrive(
                name: name,
                mountPath: path,
                isExternal: isExternal,
                totalBytes: cap,
                isOnline: true,
                matchingShard: matched
            ))
        }

        // Append offline host-cached external drives so users can manage or inspect them even while unplugged
        for shard in shards where !usedShardUuids.contains(shard.volume_uuid) {
            list.append(DiscoveredDrive(
                name: shard.volume_name,
                mountPath: shard.mount_point,
                isExternal: shard.is_external,
                totalBytes: 0,
                isOnline: shard.is_online,
                matchingShard: shard
            ))
        }

        return list
    }

    @objc private func handleAddFolder() {
        onTriggerFolderPicker()
    }

    @objc private func handleIndexDocuments() {
        let docs = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Documents").path
        ipc.startIndexing(rootPath: docs, portable: false)
        onRequestRefresh()
    }

    @objc private func handleClose() {
        window?.close()
    }

    public func numberOfRows(in tableView: NSTableView) -> Int {
        rows.count
    }

    public func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard row >= 0, row < rows.count else { return nil }
        let drive = rows[row]
        let id = NSUserInterfaceItemIdentifier("DriveRowCell")
        let cell = (tableView.makeView(withIdentifier: id, owner: self) as? DriveRowCell) ?? DriveRowCell(identifier: id)
        cell.configure(
            drive: drive,
            onIndex: { [weak self] in
                guard let self = self else { return }
                let usePortable = drive.isExternal && (self.portableToggle.state == .on)
                // For root internal Macintosh HD ("/"), index the user's home directory by default so TCC system dirs don't stall
                let targetPath = (drive.mountPath == "/")
                    ? FileManager.default.homeDirectoryForCurrentUser.path
                    : drive.mountPath
                self.ipc.startIndexing(rootPath: targetPath, portable: usePortable)
                self.onRequestRefresh()
            },
            onRemove: { [weak self] in
                guard let self = self, let uuid = drive.matchingShard?.volume_uuid else { return }
                self.ipc.removeShard(volumeUuid: uuid)
                self.onRequestRefresh()
            }
        )
        return cell
    }
}

@MainActor
private final class DriveRowCell: NSTableCellView {
    private let iconView = NSImageView()
    private let nameLabel = NSTextField(labelWithString: "")
    private let detailLabel = NSTextField(labelWithString: "")
    private let indexBtn = NSButton(title: "Index Drive", target: nil, action: nil)
    private let removeBtn = NSButton(title: "Remove", target: nil, action: nil)

    private var onIndexAction: (() -> Void)?
    private var onRemoveAction: (() -> Void)?

    init(identifier: NSUserInterfaceItemIdentifier) {
        super.init(frame: NSRect(x: 0, y: 0, width: 580, height: 64))
        self.identifier = identifier

        iconView.frame = NSRect(x: 10, y: 14, width: 36, height: 36)
        addSubview(iconView)

        nameLabel.font = .systemFont(ofSize: 14, weight: .semibold)
        nameLabel.frame = NSRect(x: 56, y: 34, width: 320, height: 18)
        addSubview(nameLabel)

        detailLabel.font = .systemFont(ofSize: 11.5)
        detailLabel.textColor = .secondaryLabelColor
        detailLabel.lineBreakMode = .byTruncatingMiddle
        detailLabel.frame = NSRect(x: 56, y: 12, width: 335, height: 18)
        addSubview(detailLabel)

        indexBtn.bezelStyle = .rounded
        indexBtn.target = self
        indexBtn.action = #selector(didTapIndex)
        indexBtn.frame = NSRect(x: 400, y: 18, width: 102, height: 28)
        addSubview(indexBtn)

        removeBtn.bezelStyle = .rounded
        removeBtn.target = self
        removeBtn.action = #selector(didTapRemove)
        removeBtn.frame = NSRect(x: 506, y: 18, width: 68, height: 28)
        addSubview(removeBtn)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    func configure(
        drive: DiscoveredDrive,
        onIndex: @escaping () -> Void,
        onRemove: @escaping () -> Void
    ) {
        self.onIndexAction = onIndex
        self.onRemoveAction = onRemove

        if drive.isOnline {
            iconView.image = NSWorkspace.shared.icon(forFile: drive.mountPath)
        } else {
            iconView.image = NSImage(systemSymbolName: "externaldrive.badge.xmark", accessibilityDescription: "Offline Drive")
        }

        let kindBadge = drive.isExternal ? "External" : "Internal"
        let sizeBadge = drive.totalBytes > 0
            ? ByteCountFormatter.string(fromByteCount: drive.totalBytes, countStyle: .file)
            : ""
        nameLabel.stringValue = sizeBadge.isEmpty
            ? "\(drive.name) (\(kindBadge))"
            : "\(drive.name) · \(sizeBadge) (\(kindBadge))"

        if let shard = drive.matchingShard {
            let countStr = NumberFormatter.localizedString(from: NSNumber(value: shard.l0_paths), number: .decimal)
            let stateStr = drive.isOnline ? "✓ Indexed (\(countStr) paths)" : "Offline Cached (\(countStr) paths)"
            let rootsInfo = (shard.indexed_roots ?? []).joined(separator: ", ")
            detailLabel.stringValue = rootsInfo.isEmpty ? stateStr : "\(stateStr) · \(rootsInfo)"
            indexBtn.title = drive.mountPath == "/" ? "Index Home" : "Re-index"
            indexBtn.isEnabled = drive.isOnline
            removeBtn.isHidden = false
        } else {
            let targetDesc = drive.mountPath == "/" ? "Indexes your Home folder (~)" : drive.mountPath
            detailLabel.stringValue = "Not indexed · \(targetDesc)"
            indexBtn.title = drive.mountPath == "/" ? "Index Home" : "Index Drive"
            indexBtn.isEnabled = drive.isOnline
            removeBtn.isHidden = true
        }
    }

    @objc private func didTapIndex() { onIndexAction?() }
    @objc private func didTapRemove() { onRemoveAction?() }
}
