import Foundation
import Darwin

public struct QueryContextPayload: Codable, Sendable {
    public var active_app_bundle_id: String?
    public var active_workspace_dir: String?
    public var now_unix_secs: Int64?

    public init(activeAppBundleId: String? = nil, activeWorkspaceDir: String? = nil) {
        self.active_app_bundle_id = activeAppBundleId
        self.active_workspace_dir = activeWorkspaceDir
        self.now_unix_secs = nil
    }
}

public struct GhostPredictionPayload: Codable, Sendable {
    public let full_query: String
    public let ghost_suffix: String
}

public struct RankedHitPayload: Codable, Sendable {
    public let doc_id: UInt64
    public let path: String
    public let filename: String
    public let rrf_score: Float
    public let snippet: String
    public let match_indices: [UInt32]
    public let matched_layers: [String]
    public let is_dir: Bool
    public let volume_name: String
    public let volume_online: Bool
}

public struct SearchResponsePayload: Codable, Sendable {
    public let query: String
    public let generation: UInt64
    public let intent: String
    public let ghost_prediction: GhostPredictionPayload?
    public let stage1_l0_us: UInt64
    public let stage2_l1_us: UInt64
    public let stage3_l2_us: UInt64
    public let total_elapsed_us: UInt64
    public let total_indexed_paths: Int
    public let hits: [RankedHitPayload]
}

public struct IndexingProgressPayload: Codable, Sendable {
    public let is_indexing: Bool
    public let phase: String
    public let active_target: String
    public let files_scanned: UInt64
    public let dirs_scanned: UInt64
    public let entries_ingested: UInt64
    public let entries_total: UInt64

    public init(
        is_indexing: Bool = false,
        phase: String = "Idle",
        active_target: String = "",
        files_scanned: UInt64 = 0,
        dirs_scanned: UInt64 = 0,
        entries_ingested: UInt64 = 0,
        entries_total: UInt64 = 0
    ) {
        self.is_indexing = is_indexing
        self.phase = phase
        self.active_target = active_target
        self.files_scanned = files_scanned
        self.dirs_scanned = dirs_scanned
        self.entries_ingested = entries_ingested
        self.entries_total = entries_total
    }
}

public struct ShardStatusPayload: Codable, Sendable {
    public let volume_name: String
    public let volume_uuid: String
    public let mount_point: String
    public let is_external: Bool
    public let is_online: Bool
    public let mode: String
    public let shard_dir: String
    public let key_backend: String
    public let l0_paths: Int
    public let l1_docs: UInt64
    public let l2_vectors: Int
    public let mlocked: Bool
    public let last_fsevent_id: UInt64
    public let indexed_roots: [String]?
}

public struct EngineStatusPayload: Codable, Sendable {
    public let app_support_dir: String
    public let secure_enclave_status: String
    public let shard_count: Int
    public let total_paths: Int
    public let shards: [ShardStatusPayload]
    public let indexing: IndexingProgressPayload
}

private struct QueryEnvelope: Encodable {
    let op = "query"
    let query: String
    let limit: Int
    let context: QueryContextPayload
    let l0_only: Bool
}

private struct RecordSelectionEnvelope: Encodable {
    let op = "record_selection"
    let query: String
    let selected_path: String
    let context: QueryContextPayload
}

private struct IndexRootEnvelope: Encodable {
    let op: String
    let root: String
    let portable: Bool
}

private struct RemoveShardEnvelope: Encodable {
    let op = "remove_shard"
    let volume_uuid: String
}

private struct SimpleOpEnvelope: Encodable {
    let op: String
}

private struct RawIpcResponse: Decodable {
    let type: String
    let query: String?
    let generation: UInt64?
    let intent: String?
    let ghost_prediction: GhostPredictionPayload?
    let stage1_l0_us: UInt64?
    let stage2_l1_us: UInt64?
    let stage3_l2_us: UInt64?
    let total_elapsed_us: UInt64?
    let total_indexed_paths: Int?
    let hits: [RankedHitPayload]?

    // Status fields
    let app_support_dir: String?
    let secure_enclave_status: String?
    let shard_count: Int?
    let total_paths: Int?
    let shards: [ShardStatusPayload]?
    let indexing: IndexingProgressPayload?

    let message: String?
}

/// Sub-millisecond Unix Domain Socket client connecting to the Rust `searchd` daemon.
public final class SearchdIPCClient: Sendable {
    public let socketPath: String

    public init(socketPath: String? = nil) {
        if let custom = socketPath ?? ProcessInfo.processInfo.environment["FMS_SOCKET"] {
            self.socketPath = custom
        } else {
            let home = FileManager.default.homeDirectoryForCurrentUser.path
            self.socketPath = "\(home)/Library/Caches/dev.fastmacsearch/searchd.sock"
        }
    }

    public func ping() -> Bool {
        guard let data = try? sendLine(encodable: SimpleOpEnvelope(op: "ping")),
              let decoded = try? JSONDecoder().decode(RawIpcResponse.self, from: data)
        else {
            return false
        }
        return decoded.type == "ack"
    }

    public func status() throws -> EngineStatusPayload {
        let data = try sendLine(encodable: SimpleOpEnvelope(op: "status"))
        let decoded = try JSONDecoder().decode(RawIpcResponse.self, from: data)
        if decoded.type == "status" {
            return EngineStatusPayload(
                app_support_dir: decoded.app_support_dir ?? "",
                secure_enclave_status: decoded.secure_enclave_status ?? "Unknown",
                shard_count: decoded.shard_count ?? 0,
                total_paths: decoded.total_paths ?? 0,
                shards: decoded.shards ?? [],
                indexing: decoded.indexing ?? IndexingProgressPayload()
            )
        }
        throw NSError(
            domain: "FastMacSearchIPC",
            code: 4,
            userInfo: [NSLocalizedDescriptionKey: decoded.message ?? "Failed to fetch engine status"]
        )
    }

    /// Triggers non-blocking background indexing in `searchd`.
    public func startIndexing(rootPath: String, portable: Bool = false) {
        let client = self
        DispatchQueue.global(qos: .utility).async {
            let asyncReq = IndexRootEnvelope(op: "index_root_async", root: rootPath, portable: portable)
            _ = try? client.sendLine(encodable: asyncReq, timeoutSeconds: 8)
        }
    }

    public func removeShard(volumeUuid: String) {
        let client = self
        DispatchQueue.global(qos: .utility).async {
            let req = RemoveShardEnvelope(volume_uuid: volumeUuid)
            _ = try? client.sendLine(encodable: req)
        }
    }

    public func query(
        _ text: String,
        limit: Int = 10,
        context: QueryContextPayload,
        l0Only: Bool = false
    ) throws -> SearchResponsePayload {
        let req = QueryEnvelope(query: text, limit: limit, context: context, l0_only: l0Only)
        let data = try sendLine(encodable: req)
        let decoded = try JSONDecoder().decode(RawIpcResponse.self, from: data)
        if decoded.type == "search",
           let q = decoded.query,
           let gen = decoded.generation,
           let intent = decoded.intent,
           let hits = decoded.hits
        {
            return SearchResponsePayload(
                query: q,
                generation: gen,
                intent: intent,
                ghost_prediction: decoded.ghost_prediction,
                stage1_l0_us: decoded.stage1_l0_us ?? 0,
                stage2_l1_us: decoded.stage2_l1_us ?? 0,
                stage3_l2_us: decoded.stage3_l2_us ?? 0,
                total_elapsed_us: decoded.total_elapsed_us ?? 0,
                total_indexed_paths: decoded.total_indexed_paths ?? 0,
                hits: hits
            )
        }
        throw NSError(
            domain: "FastMacSearchIPC",
            code: 2,
            userInfo: [NSLocalizedDescriptionKey: decoded.message ?? "Unexpected daemon response"]
        )
    }

    public func recordSelection(query: String, selectedPath: String, context: QueryContextPayload) {
        let client = self
        DispatchQueue.global(qos: .utility).async {
            let req = RecordSelectionEnvelope(query: query, selected_path: selectedPath, context: context)
            _ = try? client.sendLine(encodable: req, timeoutSeconds: 2)
        }
    }

    private func sendLine<T: Encodable>(encodable: T, timeoutSeconds: Int = 3) throws -> Data {
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else {
            throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno))
        }
        defer { Darwin.close(fd) }

        // Prevent macOS XNU from killing the GUI process with SIGPIPE if socket closes mid-write,
        // and enforce bounded send/receive timeouts so GCD threads never stall or pile up.
        var noSigPipe: Int32 = 1
        _ = Darwin.setsockopt(
            fd,
            SOL_SOCKET,
            SO_NOSIGPIPE,
            &noSigPipe,
            socklen_t(MemoryLayout<Int32>.size)
        )
        var tv = timeval(tv_sec: timeoutSeconds, tv_usec: 0)
        _ = Darwin.setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        _ = Darwin.setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let utf8Path = socketPath.utf8CString
        let maxLen = MemoryLayout.size(ofValue: addr.sun_path)
        guard utf8Path.count <= maxLen else {
            throw NSError(
                domain: "FastMacSearchIPC",
                code: 3,
                userInfo: [NSLocalizedDescriptionKey: "Socket path exceeds sun_path limit"]
            )
        }
        withUnsafeMutableBytes(of: &addr.sun_path) { rawBuf in
            utf8Path.withUnsafeBytes { srcBuf in
                rawBuf.copyBytes(from: srcBuf)
            }
        }

        let addrLen = socklen_t(MemoryLayout<sockaddr_un>.size)
        let connectRes = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, addrLen)
            }
        }
        guard connectRes == 0 else {
            throw NSError(
                domain: NSPOSIXErrorDomain,
                code: Int(errno),
                userInfo: [NSLocalizedDescriptionKey: "searchd is starting up…"]
            )
        }

        var payload = try JSONEncoder().encode(encodable)
        payload.append(0x0A)
        try payload.withUnsafeBytes { buf in
            guard let base = buf.baseAddress else { return }
            var written = 0
            while written < buf.count {
                let n = Darwin.write(fd, base.advanced(by: written), buf.count - written)
                if n <= 0 { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
                written += n
            }
        }

        var response = Data()
        var chunk = [UInt8](repeating: 0, count: 16384)
        while true {
            let n = Darwin.read(fd, &chunk, chunk.count)
            if n <= 0 { break }
            if let nlIdx = chunk[0..<n].firstIndex(of: 0x0A) {
                response.append(contentsOf: chunk[0..<nlIdx])
                break
            }
            response.append(contentsOf: chunk[0..<n])
        }
        return response
    }
}
