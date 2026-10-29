import Foundation

/// Automatically launches and supervises the embedded `searchd` Rust daemon inside `Fast Mac Search.app`
/// (`Contents/Helpers/searchd`) so users never need to run a CLI command.
@MainActor
public final class DaemonSupervisor {
    private let ipc: SearchdIPCClient
    private var managedProcess: Process?
    private var lastSpawnAttempt: Date = .distantPast

    public init(ipc: SearchdIPCClient) {
        self.ipc = ipc
    }

    public func ensureDaemonRunning() {
        // 1. Never spawn a second daemon if our managed child process is already alive/starting up!
        if let proc = managedProcess, proc.isRunning {
            return
        }

        // 2. Throttle spawn attempts (at least 5s apart) so status polling can never rapid-fire processes.
        if Date().timeIntervalSince(lastSpawnAttempt) < 5.0 {
            return
        }

        // 3. Check if an external `searchd` is already serving the socket.
        if ipc.ping() {
            return
        }

        guard let daemonURL = locateDaemonBinary() else {
            return
        }

        lastSpawnAttempt = Date()
        let proc = Process()
        proc.executableURL = daemonURL
        proc.standardOutput = FileHandle.nullDevice
        proc.standardError = FileHandle.nullDevice

        do {
            try proc.run()
            self.managedProcess = proc
        } catch {
            // Ignore if user manages searchd externally
        }
    }

    public func stopManagedDaemonIfOwned() {
        guard let proc = managedProcess, proc.isRunning else { return }
        proc.terminate()
        managedProcess = nil
    }

    private func locateDaemonBinary() -> URL? {
        let fm = FileManager.default

        // 1. Inside macOS .app bundle: Fast Mac Search.app/Contents/Helpers/searchd
        let bundleContents = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/searchd")
        if fm.isExecutableFile(atPath: bundleContents.path) {
            return bundleContents
        }

        // 2. Adjacent to executable inside Contents/MacOS/searchd
        if let execURL = Bundle.main.executableURL {
            let sibling = execURL.deletingLastPathComponent().appendingPathComponent("searchd")
            if fm.isExecutableFile(atPath: sibling.path) {
                return sibling
            }

            // 3. Development SwiftPM layout: clients/macos-ui/.build/release/FastMacSearchUI -> ../../../../target/release/searchd
            let devRelease = execURL
                .deletingLastPathComponent()
                .appendingPathComponent("../../../../target/release/searchd")
                .standardizedFileURL
            if fm.isExecutableFile(atPath: devRelease.path) {
                return devRelease
            }
        }

        // 4. User cargo bin fallback
        let cargoBin = fm.homeDirectoryForCurrentUser.appendingPathComponent(".cargo/bin/searchd")
        if fm.isExecutableFile(atPath: cargoBin.path) {
            return cargoBin
        }

        return nil
    }
}
