import Foundation
import Photos

/// Files you open or save through Apple's Files sheets stay in their own place (no copies).
/// iPadOS only allows an app to touch such a file while it holds a "security scope" for it, so
/// this class keeps those scopes open, remembers them between launches (so Recent files still
/// open), and writes changes back the way iPadOS wants (file coordination, which also keeps
/// iCloud Drive in sync).
final class FileAccess {
    static let shared = FileAccess()

    private let lock = NSLock()
    private var urls: [String: URL] = [:]
    private let key = "PhotoCraftFileBookmarks"
    private let orderKey = "PhotoCraftFileBookmarkOrder"
    private let limit = 60

    /// Call at launch, before the app starts: reopens the scopes of recently used files.
    func restore() {
        let defaults = UserDefaults.standard
        guard let stored = defaults.dictionary(forKey: key) as? [String: Data] else { return }
        var order = defaults.stringArray(forKey: orderKey) ?? []
        var restored = 0
        var fresh: [String: Data] = [:]
        var freshOrder: [String] = []
        for path in order.reversed().prefix(limit).reversed() {
            guard let data = stored[path] else { continue }
            var stale = false
            guard let url = try? URL(resolvingBookmarkData: data, options: [], relativeTo: nil, bookmarkDataIsStale: &stale) else { continue }
            _ = url.startAccessingSecurityScopedResource()
            lock.lock(); urls[url.path] = url; lock.unlock()
            let renewed = (stale ? (try? url.bookmarkData(options: [], includingResourceValuesForKeys: nil, relativeTo: nil)) : nil) ?? data
            fresh[url.path] = renewed
            freshOrder.append(url.path)
            restored += 1
        }
        order = freshOrder
        defaults.set(fresh, forKey: key)
        defaults.set(order, forKey: orderKey)
        pcLog("file access: \(restored) recent file(s) reopened")
    }

    /// Keeps a file chosen in Files usable: now, and after a restart.
    func remember(_ url: URL) {
        _ = url.startAccessingSecurityScopedResource()
        lock.lock(); urls[url.path] = url; lock.unlock()
        guard let data = try? url.bookmarkData(options: [], includingResourceValuesForKeys: nil, relativeTo: nil) else {
            pcLog("file access: no bookmark for \(url.lastPathComponent)")
            return
        }
        let defaults = UserDefaults.standard
        var stored = (defaults.dictionary(forKey: key) as? [String: Data]) ?? [:]
        var order = defaults.stringArray(forKey: orderKey) ?? []
        order.removeAll { $0 == url.path }
        order.append(url.path)
        stored[url.path] = data
        while order.count > limit {
            let old = order.removeFirst()
            stored[old] = nil
        }
        defaults.set(stored, forKey: key)
        defaults.set(order, forKey: orderKey)
    }

    /// Makes sure the file is on the iPad (iCloud Drive files may only be stored online).
    func prepareForReading(_ url: URL) {
        var error: NSError?
        NSFileCoordinator().coordinate(readingItemAt: url, options: [.withoutChanges], error: &error) { _ in }
        if let e = error { pcLog("file access: could not prepare \(url.lastPathComponent): \(e.localizedDescription)") }
    }

    /// Writes a file outside the app. Safe to call from any thread.
    func write(path: String, data: Data) -> Bool {
        lock.lock()
        let known = urls[path]
        lock.unlock()
        let url = known ?? URL(fileURLWithPath: path)
        var coordinationError: NSError?
        var written = false
        NSFileCoordinator().coordinate(writingItemAt: url, options: .forReplacing, error: &coordinationError) { target in
            do {
                try data.write(to: target)
                written = true
            } catch {
                pcLog("file access: write failed for \(url.lastPathComponent): \(error.localizedDescription)")
            }
        }
        if let e = coordinationError {
            pcLog("file access: coordination failed for \(url.lastPathComponent): \(e.localizedDescription)")
        }
        return written
    }
}

/// Pictures exported to Photos: PhotoCraft writes them to a temporary file, and once the file
/// is complete it is added to the Photos library.
final class PhotosExport {
    static let shared = PhotosExport()
    private let lock = NSLock()
    private var pending = Set<String>()

    func expect(_ path: String) {
        lock.lock(); pending.insert(path); lock.unlock()
    }

    /// Called after any file has been written; does nothing unless it was meant for Photos.
    func finished(path: String) {
        lock.lock()
        let wanted = pending.remove(path) != nil
        lock.unlock()
        guard wanted else { return }
        let url = URL(fileURLWithPath: path)
        PHPhotoLibrary.requestAuthorization(for: .addOnly) { status in
            guard status == .authorized || status == .limited else {
                pcLog("photos: not allowed (\(status.rawValue))")
                DispatchQueue.main.async {
                    NotificationCenter.default.post(name: .pcPhotosSaved, object: false, userInfo: ["error": "PhotoCraft is not allowed to add photos. Allow it in Settings › PhotoCraft › Photos."])
                }
                return
            }
            PHPhotoLibrary.shared().performChanges({
                PHAssetCreationRequest.forAsset().addResource(with: .photo, fileURL: url, options: nil)
            }) { ok, error in
                pcLog("photos: saved \(ok) \(error?.localizedDescription ?? "")")
                try? FileManager.default.removeItem(at: url.deletingLastPathComponent())
                DispatchQueue.main.async {
                    NotificationCenter.default.post(name: .pcPhotosSaved, object: ok, userInfo: ["error": error?.localizedDescription ?? ""])
                }
            }
        }
    }
}
