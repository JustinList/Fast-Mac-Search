import AppKit
import Foundation

// MARK: - Instant Live Calculator (Nanolight MIT-inspired)

public enum MathCalculator {
    public static func evaluate(_ input: String) -> CalcAnswerPayload? {
        var trimmed = input.trimmingCharacters(in: .whitespacesAndNewlines)
        while trimmed.hasSuffix("=") {
            trimmed.removeLast()
            trimmed = trimmed.trimmingCharacters(in: .whitespacesAndNewlines)
        }
        guard !trimmed.isEmpty, trimmed.utf8.count <= 256 else { return nil }

        var parser = CalcParser(trimmed)
        guard let val = parser.parseExpr() else { return nil }
        parser.skipSpaces()
        guard parser.atEnd, parser.sawMathOpOrFn, val.isFinite else { return nil }

        let cleanVal = (val == 0.0) ? 0.0 : val
        let formatted = formatNumberGrouped(cleanVal)
        let hex: String?
        if cleanVal >= 0.0, cleanVal < 1e15, cleanVal == cleanVal.rounded() {
            hex = String(format: "0x%llX", UInt64(cleanVal))
        } else {
            hex = nil
        }
        return CalcAnswerPayload(expression: trimmed, formatted: formatted, raw_value: cleanVal, hex: hex)
    }

    private static func formatNumberGrouped(_ val: Double) -> String {
        let absVal = abs(val)
        if absVal != 0.0 && (absVal < 1e-6 || absVal >= 1e15) {
            return String(format: "%g", val)
        }

        let rounded = (abs(val - val.rounded()) < 1e-11 && absVal < 1e15) ? val.rounded() : val
        let formatter = NumberFormatter()
        formatter.numberStyle = .decimal
        formatter.usesGroupingSeparator = true
        formatter.groupingSeparator = ","
        formatter.decimalSeparator = "."

        if rounded == rounded.rounded(.towardZero) && abs(rounded) < 1e15 {
            formatter.maximumFractionDigits = 0
            return formatter.string(from: NSNumber(value: rounded)) ?? String(format: "%.0f", rounded)
        } else {
            formatter.minimumFractionDigits = 0
            formatter.maximumFractionDigits = 10
            return formatter.string(from: NSNumber(value: rounded)) ?? "\(rounded)"
        }
    }

    private struct CalcParser {
        let chars: [Character]
        var i: Int = 0
        var sawMathOpOrFn: Bool = false

        init(_ s: String) {
            self.chars = Array(s)
        }

        var atEnd: Bool { i >= chars.count }

        mutating func skipSpaces() {
            while i < chars.count && (chars[i] == " " || chars[i] == "\t") {
                i += 1
            }
        }

        mutating func peek() -> Character? {
            skipSpaces()
            return i < chars.count ? chars[i] : nil
        }

        mutating func consume(_ target: Character, alt: Character? = nil) -> Bool {
            skipSpaces()
            guard i < chars.count else { return false }
            if chars[i] == target {
                i += 1
                return true
            }
            if let altChar = alt, chars[i] == altChar {
                i += 1
                return true
            }
            return false
        }

        mutating func parseExpr() -> Double? {
            guard var v = parseTerm() else { return nil }
            while true {
                if consume("+") {
                    sawMathOpOrFn = true
                    guard let rhs = parseTerm() else { return nil }
                    v += rhs
                } else if consume("-", alt: "−") {
                    sawMathOpOrFn = true
                    guard let rhs = parseTerm() else { return nil }
                    v -= rhs
                } else {
                    return v
                }
            }
        }

        mutating func parseTerm() -> Double? {
            guard var v = parsePower() else { return nil }
            while true {
                if consume("*", alt: "×") {
                    sawMathOpOrFn = true
                    guard let rhs = parsePower() else { return nil }
                    v *= rhs
                } else if consume("/", alt: "÷") {
                    sawMathOpOrFn = true
                    guard let rhs = parsePower(), rhs != 0.0 else { return nil }
                    v /= rhs
                } else if consume("%") {
                    sawMathOpOrFn = true
                    guard let rhs = parsePower(), rhs != 0.0 else { return nil }
                    v = v.truncatingRemainder(dividingBy: rhs)
                } else {
                    return v
                }
            }
        }

        /// Right-associative exponentiation (`2^3^2 == 512`).
        mutating func parsePower() -> Double? {
            guard let base = parseUnary() else { return nil }
            if consume("^") {
                sawMathOpOrFn = true
                guard let exp = parsePower() else { return nil }
                return pow(base, exp)
            }
            return base
        }

        mutating func parseUnary() -> Double? {
            if consume("-", alt: "−") {
                guard let v = parseUnary() else { return nil }
                return -v
            }
            if consume("+") {
                return parseUnary()
            }
            return parsePrimary()
        }

        mutating func parsePrimary() -> Double? {
            guard let ch = peek() else { return nil }
            if ch == "π" {
                i += 1
                return Double.pi
            }
            if ch == "(" {
                i += 1
                guard let v = parseExpr(), consume(")") else { return nil }
                return v
            }
            if ch == "0", i + 1 < chars.count, (chars[i + 1] == "x" || chars[i + 1] == "X") {
                i += 2
                let start = i
                while i < chars.count, (chars[i].isHexDigit || chars[i] == "_") {
                    i += 1
                }
                let hexStr = String(chars[start..<i].filter { $0 != "_" })
                guard !hexStr.isEmpty, let u = UInt64(hexStr, radix: 16) else { return nil }
                return Double(u)
            }
            if ch.isNumber || ch == "." {
                return parseNumber()
            }
            if ch.isLetter {
                return parseWordOrFunction()
            }
            return nil
        }

        mutating func parseNumber() -> Double? {
            let start = i
            while i < chars.count && (chars[i].isNumber || chars[i] == "." || chars[i] == "_") {
                i += 1
            }
            if i + 1 < chars.count && (chars[i] == "e" || chars[i] == "E") {
                let next = chars[i + 1]
                let validSci = next.isNumber
                    || ((next == "-" || next == "+") && i + 2 < chars.count && chars[i + 2].isNumber)
                if validSci {
                    i += 2
                    while i < chars.count && chars[i].isNumber {
                        i += 1
                    }
                }
            }
            let numStr = String(chars[start..<i].filter { $0 != "_" })
            return Double(numStr)
        }

        mutating func parseWordOrFunction() -> Double? {
            let start = i
            while i < chars.count && (chars[i].isLetter || chars[i].isNumber || chars[i] == "_") {
                i += 1
            }
            let ident = String(chars[start..<i]).lowercased()
            switch ident {
            case "pi": return Double.pi
            case "e": return M_E
            case "tau": return Double.pi * 2.0
            default: break
            }

            let arg: Double
            if peek() == "(" {
                i += 1
                guard let v = parseExpr(), consume(")") else { return nil }
                arg = v
            } else {
                guard let v = parseUnary() else { return nil }
                arg = v
            }

            sawMathOpOrFn = true
            let out: Double
            switch ident {
            case "sqrt":
                guard arg >= 0 else { return nil }
                out = sqrt(arg)
            case "sin": out = sin(arg)
            case "cos": out = cos(arg)
            case "tan": out = tan(arg)
            case "asin": out = asin(arg)
            case "acos": out = acos(arg)
            case "atan": out = atan(arg)
            case "ln":
                guard arg > 0 else { return nil }
                out = log(arg)
            case "log", "log10":
                guard arg > 0 else { return nil }
                out = log10(arg)
            case "log2":
                guard arg > 0 else { return nil }
                out = log2(arg)
            case "abs": out = abs(arg)
            case "floor": out = floor(arg)
            case "ceil": out = ceil(arg)
            case "round": out = arg.rounded()
            default: return nil
            }
            return out.isFinite ? out : nil
        }
    }
}

// MARK: - Nanolight 7-Tier Matcher (Initials, CamelCase, Subsequence, & Damerau-Levenshtein Typos)

public struct LauncherMatch: Sendable {
    public let score: Int
    public let matchedCharIndices: [Int]

    public init(score: Int, matchedCharIndices: [Int]) {
        self.score = score
        self.matchedCharIndices = matchedCharIndices
    }
}

public enum LauncherMatcher {
    public static func score(query rawQuery: String, name: String) -> LauncherMatch? {
        let query = rawQuery.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !query.isEmpty, !name.isEmpty else { return nil }

        let qChars = Array(query)
        let nChars = Array(name)
        let qLower: [String] = qChars.map { $0.lowercased() }
        let nLower: [String] = nChars.map { $0.lowercased() }

        let slack = min(max(0, nChars.count - qChars.count), 60)

        // Tier 1: Exact whole name
        if qLower == nLower {
            return LauncherMatch(score: 1000, matchedCharIndices: Array(0..<nChars.count))
        }

        // Tier 2: Prefix of name
        if qLower.count <= nLower.count && nLower.starts(with: qLower) {
            return LauncherMatch(score: 900 - slack, matchedCharIndices: Array(0..<qChars.count))
        }

        // Tier 3: Word-start or CamelCase boundary ("code" in Visual Studio Code, "time" in FaceTime)
        if qLower.count < nLower.count {
            let maxStart = nLower.count - qLower.count
            for start in 1..<(maxStart + 1) {
                if isWordStart(nChars, at: start) && sliceEquals(nLower, from: start, needle: qLower) {
                    return LauncherMatch(
                        score: 800 - slack - min(start, 60),
                        matchedCharIndices: Array(start..<(start + qLower.count))
                    )
                }
            }
        }

        // Tier 4: Word Initials / Acronyms ("ss" -> System Settings, "vsc" -> Visual Studio Code)
        if let acronymMatch = matchInitials(qLower: qLower, nChars: nChars, nLower: nLower, slack: slack) {
            return acronymMatch
        }

        // Tier 5: Substring anywhere
        if qLower.count <= nLower.count {
            let maxStart = nLower.count - qLower.count
            for start in 0..<(maxStart + 1) {
                if sliceEquals(nLower, from: start, needle: qLower) {
                    return LauncherMatch(
                        score: 600 - slack - min(start, 60),
                        matchedCharIndices: Array(start..<(start + qLower.count))
                    )
                }
            }
        }

        // Tier 6: Ordered subsequence (ignoring spaces in query)
        if let subseq = matchSubsequence(qLower: qLower, nChars: nChars, nLower: nLower, slack: slack) {
            return subseq
        }

        // Tier 7: Damerau-Levenshtein Typo Tolerance ("chorme" -> Chrome, "calandar" -> Calendar)
        return matchTypo(qLower: qLower, nChars: nChars, nLower: nLower, slack: slack)
    }

    private static func isWordStart(_ chars: [Character], at i: Int) -> Bool {
        if i <= 0 { return true }
        guard i < chars.count else { return false }
        let prev = chars[i - 1]
        let cur = chars[i]
        if prev == " " || prev == "-" || prev == "_" || prev == "." || prev == "/" || prev == "(" {
            return cur != " "
        }
        return cur.isUppercase && prev.isLowercase
    }

    private static func sliceEquals(_ hay: [String], from start: Int, needle: [String]) -> Bool {
        guard start >= 0, start + needle.count <= hay.count else { return false }
        for k in 0..<needle.count where hay[start + k] != needle[k] {
            return false
        }
        return true
    }

    private static func matchInitials(
        qLower: [String],
        nChars: [Character],
        nLower: [String],
        slack: Int
    ) -> LauncherMatch? {
        let qCompact = qLower.filter { $0 != " " }
        guard qCompact.count >= 2 else { return nil }

        var wordStarts: [Int] = []
        for idx in 0..<nChars.count where isWordStart(nChars, at: idx) {
            wordStarts.append(idx)
        }
        guard wordStarts.count >= qCompact.count else { return nil }

        var matched: [Int] = []
        var wi = 0
        for qch in qCompact {
            var found = false
            while wi < wordStarts.count {
                let pos = wordStarts[wi]
                wi += 1
                if nLower[pos] == qch {
                    matched.append(pos)
                    found = true
                    break
                }
            }
            if !found { return nil }
        }
        return LauncherMatch(score: 740 - slack, matchedCharIndices: matched)
    }

    private static func matchSubsequence(
        qLower: [String],
        nChars: [Character],
        nLower: [String],
        slack: Int
    ) -> LauncherMatch? {
        let qCompact = qLower.filter { $0 != " " }
        guard !qCompact.isEmpty, qCompact.count <= nLower.count else { return nil }

        var matched: [Int] = []
        matched.reserveCapacity(qCompact.count)
        var ni = 0
        var wordBoundaryHits = 0

        for qch in qCompact {
            var foundIdx: Int?
            var scan = ni
            while scan < nLower.count {
                if nLower[scan] == qch {
                    if foundIdx == nil { foundIdx = scan }
                    if isWordStart(nChars, at: scan) {
                        foundIdx = scan
                        break
                    }
                }
                scan += 1
            }
            guard let chosen = foundIdx else { return nil }
            if isWordStart(nChars, at: chosen) { wordBoundaryHits += 1 }
            matched.append(chosen)
            ni = chosen + 1
        }

        guard let first = matched.first, let last = matched.last else { return nil }
        let span = max(0, (last - first + 1) - qCompact.count)
        let startPenalty = min(first * 3, 45)
        let spreadPenalty = min(span * 8, 180)
        let boundaryBonus = wordBoundaryHits * 18
        let pts = max(140, 460 + boundaryBonus - spreadPenalty - startPenalty - slack / 2)
        return LauncherMatch(score: pts, matchedCharIndices: matched)
    }

    private static func allowedTypos(for length: Int) -> Int {
        if length < 4 { return 0 }
        if length < 7 { return 1 }
        return 2
    }

    private static func matchTypo(
        qLower: [String],
        nChars: [Character],
        nLower: [String],
        slack: Int
    ) -> LauncherMatch? {
        let maxErrs = allowedTypos(for: qLower.count)
        guard maxErrs > 0, qLower.count <= 32 else { return nil }

        var best: (errs: Int, start: Int, matched: [Int])?

        for s in 0..<nChars.count where isWordStart(nChars, at: s) {
            let wordLen = min(nLower.count - s, 48)
            guard wordLen >= max(2, qLower.count - maxErrs) else { continue }
            let word = Array(nLower[s..<(s + wordLen)])
            if let (errs, localHits) = damerauPrefixDistance(query: qLower, word: word, maxErrs: maxErrs) {
                if best == nil || errs < (best?.errs ?? Int.max) {
                    let globalHits = localHits.map { s + $0 }
                    best = (errs, s, globalHits)
                    if errs == 1 && s == 0 { break }
                }
            }
        }

        guard let b = best else { return nil }
        let pts = max(80, 320 - b.errs * 70 - min(b.start, 40) - slack / 2)
        return LauncherMatch(score: pts, matchedCharIndices: b.matched)
    }

    private static func damerauPrefixDistance(
        query q: [String],
        word w: [String],
        maxErrs: Int
    ) -> (Int, [Int])? {
        let m = q.count
        let n = w.count
        guard m > 0, n > 0 else { return nil }

        var dp = Array(repeating: Array(repeating: 0, count: n + 1), count: m + 1)
        for i in 0..<(m + 1) { dp[i][0] = i }
        for j in 0..<(n + 1) { dp[0][j] = j }

        for i in 1..<(m + 1) {
            for j in 1..<(n + 1) {
                let cost = (q[i - 1] == w[j - 1]) ? 0 : 1
                var v = min(
                    dp[i - 1][j] + 1,
                    min(
                        dp[i][j - 1] + 1,
                        dp[i - 1][j - 1] + cost
                    )
                )
                if i > 1 && j > 1 && q[i - 1] == w[j - 2] && q[i - 2] == w[j - 1] {
                    v = min(v, dp[i - 2][j - 2] + 1)
                }
                dp[i][j] = v
            }
        }

        var bestErrs = maxErrs + 1
        var bestEnd = 0
        let minJ = max(1, m - maxErrs)
        let maxJ = min(n, m + maxErrs)
        guard minJ <= maxJ else { return nil }

        for j in minJ..<(maxJ + 1) {
            if dp[m][j] < bestErrs {
                bestErrs = dp[m][j]
                bestEnd = j
            }
        }
        guard bestErrs <= maxErrs else { return nil }

        var hits: [Int] = []
        var i = m
        var j = bestEnd
        while i > 0 && j > 0 {
            let cur = dp[i][j]
            if q[i - 1] == w[j - 1] && cur == dp[i - 1][j - 1] {
                hits.append(j - 1)
                i -= 1
                j -= 1
            } else if i > 1 && j > 1 && q[i - 1] == w[j - 2] && q[i - 2] == w[j - 1] && cur == dp[i - 2][j - 2] + 1 {
                hits.append(j - 1)
                hits.append(j - 2)
                i -= 2
                j -= 2
            } else if cur == dp[i - 1][j - 1] + 1 {
                i -= 1
                j -= 1
            } else if cur == dp[i - 1][j] + 1 {
                i -= 1
            } else {
                j -= 1
            }
        }
        hits.sort()
        return (bestErrs, hits)
    }

    /// Builds an `NSAttributedString` where matched character positions render brighter & bolder,
    /// mirroring Nanolight's tactile matched-letter highlighting across Apps, Files, and AI Chats.
    public static func attributedHighlightedTitle(
        _ text: String,
        matchedCharIndices: [Int],
        baseFont: NSFont,
        boldFont: NSFont,
        baseColor: NSColor,
        highlightColor: NSColor
    ) -> NSAttributedString {
        let attr = NSMutableAttributedString(
            string: text,
            attributes: [
                .font: baseFont,
                .foregroundColor: baseColor
            ]
        )
        guard !matchedCharIndices.isEmpty else { return attr }

        let hitSet = Set(matchedCharIndices)
        var utf16Offset = 0
        for (charIdx, ch) in text.enumerated() {
            let u16Len = ch.utf16.count
            if hitSet.contains(charIdx) {
                let range = NSRange(location: utf16Offset, length: u16Len)
                attr.addAttributes(
                    [
                        .font: boldFont,
                        .foregroundColor: highlightColor
                    ],
                    range: range
                )
            }
            utf16Offset += u16Len
        }
        return attr
    }
}

// MARK: - First-Class macOS Application Launcher Index

public struct DiscoveredApp: Sendable {
    public let name: String
    public let path: String
    public let folder: String
}

public struct RankedAppHit: Sendable {
    public let app: DiscoveredApp
    public let score: Int
    public let launches: Int
    public let matchedCharIndices: [Int]
}

@MainActor
public final class AppLauncherIndex {
    private static let launchCountsDefaultsKey = "FMSAppLaunchCounts"
    private static let starterNames = ["Safari", "Notes", "Calendar", "System Settings", "Finder", "Terminal"]

    private var apps: [DiscoveredApp] = []
    private var launchCounts: [String: Int] = [:]
    private var iconCache: [String: NSImage] = [:]
    private var lastRescanDate: Date = .distantPast

    public var appCount: Int { apps.count }

    public init() {
        if let saved = UserDefaults.standard.dictionary(forKey: Self.launchCountsDefaultsKey) as? [String: Int] {
            self.launchCounts = saved
        }
        rescanIfNeeded(force: true)
    }

    public func rescanIfNeeded(force: Bool = false) {
        if !force && Date().timeIntervalSince(lastRescanDate) < 60.0 && !apps.isEmpty {
            return
        }
        lastRescanDate = Date()

        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let roots = [
            "/Applications",
            "/System/Applications",
            "/System/Library/CoreServices/Applications",
            "\(home)/Applications"
        ]

        var seenLower = Set<String>()
        var discovered: [DiscoveredApp] = []

        func registerApp(name: String, fullPath: String) {
            let key = name.lowercased()
            guard !key.isEmpty, !seenLower.contains(key) else { return }
            seenLower.insert(key)
            let parent = ((fullPath as NSString).deletingLastPathComponent as NSString).lastPathComponent
            discovered.append(DiscoveredApp(name: name, path: fullPath, folder: parent))
        }

        let fm = FileManager.default
        for root in roots {
            scanDirectory(root, depth: 1, fm: fm, register: registerApp)
        }
        if fm.fileExists(atPath: "/System/Library/CoreServices/Finder.app") {
            registerApp(name: "Finder", fullPath: "/System/Library/CoreServices/Finder.app")
        }

        discovered.sort { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
        self.apps = discovered
    }

    private func scanDirectory(
        _ dirPath: String,
        depth: Int,
        fm: FileManager,
        register: (String, String) -> Void
    ) {
        guard let entries = try? fm.contentsOfDirectory(atPath: dirPath) else { return }
        for entry in entries {
            guard !entry.isEmpty, !entry.hasPrefix(".") else { continue }
            let full = (dirPath as NSString).appendingPathComponent(entry)
            if entry.hasSuffix(".app") {
                let appName = String(entry.dropLast(4))
                register(appName, full)
            } else if depth > 0 {
                var isDir: ObjCBool = false
                if fm.fileExists(atPath: full, isDirectory: &isDir), isDir.boolValue {
                    scanDirectory(full, depth: depth - 1, fm: fm, register: register)
                }
            }
        }
    }

    public func icon(for appPath: String) -> NSImage {
        if let cached = iconCache[appPath] {
            return cached
        }
        let img = NSWorkspace.shared.icon(forFile: appPath)
        img.size = NSSize(width: 30, height: 30)
        if iconCache.count > 200 {
            iconCache.removeAll(keepingCapacity: true)
        }
        iconCache[appPath] = img
        return img
    }

    public func recordLaunch(of app: DiscoveredApp) {
        let key = app.name.lowercased()
        let next = min(65_000, (launchCounts[key] ?? 0) + 1)
        launchCounts[key] = next
        UserDefaults.standard.set(launchCounts, forKey: Self.launchCountsDefaultsKey)
    }

    /// Top apps shown immediately when the panel opens with an empty query (most-launched first,
    /// falling back to clean system starter apps).
    public func suggestedApps(limit: Int = 5) -> [RankedAppHit] {
        guard !apps.isEmpty else { return [] }
        var out: [RankedAppHit] = []
        var usedIndices = Set<Int>()

        // 1. Frequently launched apps in descending count order
        let launched = apps.enumerated()
            .compactMap { idx, app -> (Int, DiscoveredApp, Int)? in
                let cnt = launchCounts[app.name.lowercased()] ?? 0
                return cnt > 0 ? (idx, app, cnt) : nil
            }
            .sorted { a, b in
                if a.2 != b.2 { return a.2 > b.2 }
                return a.1.name.localizedCaseInsensitiveCompare(b.1.name) == .orderedAscending
            }

        for (idx, app, cnt) in launched.prefix(limit) {
            usedIndices.insert(idx)
            out.append(RankedAppHit(app: app, score: 1000 + cnt, launches: cnt, matchedCharIndices: []))
        }

        // 2. Fill remaining slots with starter apps
        if out.count < limit {
            for starter in Self.starterNames where out.count < limit {
                if let (idx, app) = apps.enumerated().first(where: { $0.element.name.caseInsensitiveCompare(starter) == .orderedSame }),
                   !usedIndices.contains(idx)
                {
                    usedIndices.insert(idx)
                    out.append(RankedAppHit(app: app, score: 500, launches: 0, matchedCharIndices: []))
                }
            }
        }
        return out
    }

    /// Ranks installed macOS applications in `< 0.05 ms` using Nanolight's multi-tier matcher + launch boost.
    public func rankApps(query: String, limit: Int = 5) -> [RankedAppHit] {
        let trimmed = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return [] }

        var matches: [RankedAppHit] = []
        matches.reserveCapacity(16)

        for app in apps {
            guard let m = LauncherMatcher.score(query: trimmed, name: app.name) else { continue }
            let launches = launchCounts[app.name.lowercased()] ?? 0
            let boost = min(launches, 150) * 6
            matches.append(
                RankedAppHit(
                    app: app,
                    score: m.score + boost,
                    launches: launches,
                    matchedCharIndices: m.matchedCharIndices
                )
            )
        }

        matches.sort { a, b in
            if a.score != b.score { return a.score > b.score }
            if a.launches != b.launches { return a.launches > b.launches }
            return a.app.name.localizedCaseInsensitiveCompare(b.app.name) == .orderedAscending
        }

        if matches.count > limit {
            return Array(matches.prefix(limit))
        }
        return matches
    }
}

// MARK: - Opt-In Privacy-Guarded Web Suggestions (Default OFF for Zero-Trust)

@MainActor
public final class WebSuggestClient {
    public static let defaultsKey = "FMSWebSuggestionsEnabled"

    public static var isEnabled: Bool {
        get { UserDefaults.standard.bool(forKey: defaultsKey) } // Defaults to FALSE (100% local zero-trust)
        set { UserDefaults.standard.set(newValue, forKey: defaultsKey) }
    }

    private let session: URLSession
    private var cache: [String: [String]] = [:]
    private var debounceWorkItem: DispatchWorkItem?
    private var activeTask: URLSessionDataTask?

    public init() {
        let cfg = URLSessionConfiguration.ephemeral
        cfg.httpShouldSetCookies = false
        cfg.httpCookieAcceptPolicy = .never
        cfg.urlCache = nil
        cfg.timeoutIntervalForRequest = 2.0
        self.session = URLSession(configuration: cfg)
    }

    /// Nanolight's strict privacy filter: never sends local paths, filenames with extensions,
    /// URLs, hosts, emails, or `@provider` tags.
    public static func isEligible(_ raw: String) -> Bool {
        let t = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if t.utf8.count < 2 || t.utf8.count > 120 { return false }
        let lower = t.lowercased()
        if lower.hasPrefix("www.") || lower.hasPrefix("localhost") { return false }
        for scalar in t.unicodeScalars {
            let v = scalar.value
            if v <= 0x1F || v == 0x7F { return false }
            switch scalar {
            case ":", "@", "\\", "/", ".":
                return false
            default:
                break
            }
        }
        return true
    }

    nonisolated public static func googleSearchURL(for query: String) -> URL? {
        var comps = URLComponents(string: "https://www.google.com/search")
        comps?.queryItems = [URLQueryItem(name: "q", value: query)]
        return comps?.url
    }

    nonisolated public static func askGrokURL(for query: String) -> URL? {
        var comps = URLComponents(string: "https://grok.com/")
        comps?.queryItems = [URLQueryItem(name: "q", value: query)]
        return comps?.url
    }

    nonisolated public static func searchXURL(for query: String) -> URL? {
        var comps = URLComponents(string: "https://x.com/search")
        comps?.queryItems = [URLQueryItem(name: "q", value: query)]
        return comps?.url
    }

    /// Returns cached or prefix-filtered fallback completions immediately so rows never blink,
    /// and schedules a 90ms debounced cookie-free request when eligible.
    public func completions(
        for query: String,
        onUpdate: @escaping @MainActor (String, [String]) -> Void
    ) -> [String] {
        debounceWorkItem?.cancel()
        activeTask?.cancel()

        guard Self.isEnabled, Self.isEligible(query) else {
            return []
        }

        let norm = Self.normalize(query)
        if let exact = cache[norm] {
            return exact
        }

        // Prefix stand-in from shorter cached queries so rows stay rock-steady while typing
        var standIn: [String] = []
        for len in stride(from: norm.count - 1, through: 2, by: -1) {
            let prefix = String(norm.prefix(len))
            if let prev = cache[prefix] {
                standIn = prev.filter { Self.normalize($0).hasPrefix(norm) && Self.normalize($0) != norm }
                if !standIn.isEmpty { break }
            }
        }

        let work = DispatchWorkItem { [weak self] in
            Task { @MainActor in
                self?.fetchFromEndpoint(rawQuery: query, normalizedQuery: norm, onUpdate: onUpdate)
            }
        }
        debounceWorkItem = work
        DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(90), execute: work)
        return Array(standIn.prefix(4))
    }

    private func fetchFromEndpoint(
        rawQuery: String,
        normalizedQuery: String,
        onUpdate: @escaping @MainActor (String, [String]) -> Void
    ) {
        guard Self.isEnabled, Self.isEligible(rawQuery) else { return }
        var comps = URLComponents(string: "https://suggestqueries.google.com/complete/search")
        comps?.queryItems = [
            URLQueryItem(name: "client", value: "firefox"),
            URLQueryItem(name: "q", value: rawQuery)
        ]
        guard let url = comps?.url else { return }

        let task = session.dataTask(with: url) { [weak self] data, _, _ in
            guard let data = data, data.count <= 32 * 1024,
                  let parsed = Self.decodeSuggestions(data: data, expectedNorm: normalizedQuery)
            else { return }

            DispatchQueue.main.async {
                guard let self = self else { return }
                if self.cache.count > 128 {
                    self.cache.removeAll(keepingCapacity: true)
                }
                self.cache[normalizedQuery] = parsed
                onUpdate(rawQuery, parsed)
            }
        }
        activeTask = task
        task.resume()
    }

    private nonisolated static func normalize(_ s: String) -> String {
        s.trimmingCharacters(in: .whitespacesAndNewlines)
            .lowercased()
            .split(whereSeparator: { $0.isWhitespace })
            .joined(separator: " ")
    }

    private nonisolated static func decodeSuggestions(data: Data, expectedNorm: String) -> [String]? {
        guard let root = try? JSONSerialization.jsonObject(with: data) as? [Any],
              root.count >= 2,
              let echoed = root[0] as? String,
              normalize(echoed) == expectedNorm,
              let choices = root[1] as? [Any]
        else { return nil }

        var out: [String] = []
        for item in choices {
            guard let text = (item as? String)?.trimmingCharacters(in: .whitespacesAndNewlines),
                  !text.isEmpty,
                  text.utf8.count <= 120
            else { continue }
            let n = normalize(text)
            if n == expectedNorm || out.contains(where: { normalize($0) == n }) {
                continue
            }
            out.append(text)
            if out.count == 4 { break }
        }
        return out
    }
}
