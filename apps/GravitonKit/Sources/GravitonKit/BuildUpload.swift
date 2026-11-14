import Foundation

/// Sending a build folder to another Mac, as `grav build --host` does: the
/// files `.dockerignore` keeps (plus the Dockerfile and `.dockerignore`), as
/// a tar the remote gravitond unpacks and builds from.
public enum BuildUpload {
    /// Write `folder` as a tar to a temporary file; returns it and its size.
    public static func pack(_ folder: URL, dockerfile: String?) throws -> (URL, UInt64) {
        let ignoreLines = (try? String(contentsOf: folder.appending(path: ".dockerignore"), encoding: .utf8))?
            .components(separatedBy: .newlines) ?? []
        let ignore = DockerIgnore(ignoreLines)
        let out = FileManager.default.temporaryDirectory.appending(path: "graviton-build-\(UUID().uuidString).tar")
        FileManager.default.createFile(atPath: out.path, contents: nil)
        let handle = try FileHandle(forWritingTo: out)
        defer { try? handle.close() }
        var tar = TarWriter(handle: handle)

        var included = Set<String>()
        try walk(folder, "", ignore: ignore) { rel in
            try tar.append(folder.appending(path: rel), as: rel)
            included.insert(rel)
        }
        for forced in [dockerfile ?? "Dockerfile", ".dockerignore"] where !included.contains(forced) {
            let url = folder.appending(path: forced)
            if FileManager.default.fileExists(atPath: url.path) { try tar.append(url, as: forced) }
        }
        try tar.finish()
        let size = (try FileManager.default.attributesOfItem(atPath: out.path)[.size] as? NSNumber)?.uint64Value ?? 0
        return (out, size)
    }

    /// Depth-first, not following symlinks, skipping ignored paths (and what's under them).
    static func walk(_ root: URL, _ rel: String, ignore: DockerIgnore, visit: (String) throws -> Void) throws {
        let dir = rel.isEmpty ? root : root.appending(path: rel)
        for name in try FileManager.default.contentsOfDirectory(atPath: dir.path).sorted() {
            let child = rel.isEmpty ? name : "\(rel)/\(name)"
            if ignore.isIgnored(child) { continue }
            try visit(child)
            let attrs = try FileManager.default.attributesOfItem(atPath: root.appending(path: child).path)
            if attrs[.type] as? FileAttributeType == .typeDirectory {
                try walk(root, child, ignore: ignore, visit: visit)
            }
        }
    }
}

/// `.dockerignore` rules, matching `graviton_proto::build::Ignore` in Rust.
struct DockerIgnore {
    private var rules: [(negate: Bool, parts: [String])] = []

    init(_ lines: [String]) {
        for raw in lines {
            let line = raw.trimmingCharacters(in: .whitespaces)
            guard !line.isEmpty, !line.hasPrefix("#") else { continue }
            let negate = line.hasPrefix("!")
            let parts = Self.components(negate ? String(line.dropFirst()).trimmingCharacters(in: .whitespaces) : line)
            if !parts.isEmpty { rules.append((negate, parts)) }
        }
    }

    static func components(_ path: String) -> [String] {
        var out: [String] = []
        for part in path.split(separator: "/") {
            switch part {
            case ".", "": continue
            case "..": _ = out.popLast()
            default: out.append(String(part))
            }
        }
        return out
    }

    func isIgnored(_ rel: String) -> Bool {
        let parts = Self.components(rel)
        var ignored = false
        for rule in rules where (1...max(parts.count, 1)).contains(where: { parts.count >= $0 && Self.matches(rule.parts[...], parts[..<$0]) }) {
            ignored = !rule.negate
        }
        return ignored
    }

    private static func matches(_ rule: ArraySlice<String>, _ parts: ArraySlice<String>) -> Bool {
        guard let first = rule.first else { return parts.isEmpty }
        if first == "**" {
            return (0...parts.count).contains { matches(rule.dropFirst(), parts.dropFirst($0)) }
        }
        guard let head = parts.first else { return false }
        return glob(first, head) && matches(rule.dropFirst(), parts.dropFirst())
    }

    /// One path component against `*`, `?`, `[a-z]` / `[!a]` and `\x`.
    static func glob(_ pattern: String, _ name: String) -> Bool {
        func go(_ p: ArraySlice<Character>, _ n: ArraySlice<Character>) -> Bool {
            guard let c = p.first else { return n.isEmpty }
            switch c {
            case "*": return (0...n.count).contains { go(p.dropFirst(), n.dropFirst($0)) }
            case "?": return !n.isEmpty && go(p.dropFirst(), n.dropFirst())
            case "[":
                guard let close = p.dropFirst().firstIndex(of: "]") else {
                    return n.first == "[" && go(p.dropFirst(), n.dropFirst())
                }
                guard let ch = n.first else { return false }
                var cls = Array(p[p.index(after: p.startIndex)..<close])
                let negate = cls.first == "!" || cls.first == "^"
                if negate { cls.removeFirst() }
                var hit = false, i = 0
                while i < cls.count {
                    if i + 2 < cls.count, cls[i + 1] == "-" {
                        hit = hit || (cls[i] <= ch && ch <= cls[i + 2]); i += 3
                    } else {
                        hit = hit || cls[i] == ch; i += 1
                    }
                }
                return hit != negate && go(p[p.index(after: close)...], n.dropFirst())
            case "\\" where p.count > 1:
                return n.first == p.dropFirst().first && go(p.dropFirst(2), n.dropFirst())
            default:
                return n.first == c && go(p.dropFirst(), n.dropFirst())
            }
        }
        return go(Array(pattern)[...], Array(name)[...])
    }
}

/// A minimal ustar writer (GNU long names), enough for build folders:
/// files, folders and symlinks, with their modes.
struct TarWriter {
    let handle: FileHandle

    mutating func append(_ url: URL, as name: String) throws {
        let attrs = try FileManager.default.attributesOfItem(atPath: url.path)
        let mode = (attrs[.posixPermissions] as? NSNumber)?.intValue ?? 0o644
        let mtime = Int((attrs[.modificationDate] as? Date)?.timeIntervalSince1970 ?? 0)
        switch attrs[.type] as? FileAttributeType {
        case .typeDirectory?:
            try header(name: name + "/", mode: mode, size: 0, mtime: mtime, type: "5")
        case .typeSymbolicLink?:
            let target = try FileManager.default.destinationOfSymbolicLink(atPath: url.path)
            if target.utf8.count > 100 { try longEntry(type: "K", value: target) }
            try header(name: name, mode: mode, size: 0, mtime: mtime, type: "2", link: target)
        case .typeRegular?:
            let size = (attrs[.size] as? NSNumber)?.intValue ?? 0
            try header(name: name, mode: mode, size: size, mtime: mtime, type: "0")
            let source = try FileHandle(forReadingFrom: url)
            defer { try? source.close() }
            var written = 0
            while let chunk = try source.read(upToCount: 1 << 20), !chunk.isEmpty {
                try handle.write(contentsOf: chunk)
                written += chunk.count
            }
            guard written == size else { throw EngineError.io("\(name) changed while it was being sent") }
            try pad(size)
        default:
            break // sockets, devices: not part of a build
        }
    }

    func finish() throws {
        try handle.write(contentsOf: Data(count: 1024))
    }

    private func longEntry(type: Character, value: String) throws {
        let bytes = Array(value.utf8) + [0]
        try header(name: "././@LongLink", mode: 0, size: bytes.count, mtime: 0, type: type)
        try handle.write(contentsOf: Data(bytes))
        try pad(bytes.count)
    }

    private func header(name: String, mode: Int, size: Int, mtime: Int, type: Character, link: String = "") throws {
        if name.utf8.count > 100 { try longEntry(type: "L", value: name) }
        var h = [UInt8](repeating: 0, count: 512)
        func put(_ s: String, _ at: Int, _ len: Int) {
            for (i, b) in s.utf8.prefix(len).enumerated() { h[at + i] = b }
        }
        func octal(_ v: Int, _ at: Int, _ len: Int) { put(String(String(v, radix: 8).suffix(len - 1)).leftPad(len - 1), at, len - 1) }
        put(name, 0, 100)
        octal(mode & 0o7777, 100, 8)
        octal(0, 108, 8)
        octal(0, 116, 8)
        octal(size, 124, 12)
        octal(mtime, 136, 12)
        put(String(type), 156, 1)
        put(link, 157, 100)
        put("ustar ", 257, 6) // GNU magic, so readers accept the long-name entries
        put(" ", 263, 2)
        for i in 148..<156 { h[i] = 0x20 }
        let sum = h.reduce(0) { $0 + Int($1) }
        put(String(String(sum, radix: 8).leftPad(6)), 148, 6)
        h[154] = 0
        h[155] = 0x20
        try handle.write(contentsOf: Data(h))
    }

    private func pad(_ size: Int) throws {
        let rest = (512 - size % 512) % 512
        if rest > 0 { try handle.write(contentsOf: Data(count: rest)) }
    }
}

private extension String {
    func leftPad(_ width: Int) -> String {
        count >= width ? self : String(repeating: "0", count: width - count) + self
    }
}
