import UIKit
import UniformTypeIdentifiers
import PhotosUI

/// Writes a line to PhotoCraft-log.txt (visible in Files: On My iPad > PhotoCraft).
func pcLog(_ message: String) {
    guard let docs = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first else { return }
    let url = docs.appendingPathComponent("PhotoCraft-log.txt")
    let line = "[swift] \(message)\n"
    if let handle = try? FileHandle(forWritingTo: url) {
        handle.seekToEndOfFile()
        handle.write(Data(line.utf8))
        try? handle.close()
    } else {
        try? line.write(to: url, atomically: true, encoding: .utf8)
    }
}

/// Owns the drawing view and everything that needs Apple's own screens: the import chooser
/// (Photos or Files), the Files sheets, alerts, the font picker, and the bar of big buttons.
///
/// Files are never copied. A file opened from Files is read and saved in its own place, and
/// Save As asks Files where to put the new file. Nothing is kept in a hidden folder.
final class PhotoCraftViewController: UIViewController, UIDocumentPickerDelegate, UIFontPickerViewControllerDelegate, PHPickerViewControllerDelegate {

    private let metal = MetalView()

    private enum Mode { case open, save }
    private var mode: Mode = .open
    /// True when the Open sheet hands back the files themselves (copies), false when it opens them in place.
    private var openAsCopy = false
    /// The empty file Save to Files moves into place.
    private var savePlaceholder: URL?

    override var prefersStatusBarHidden: Bool { true }
    override var prefersHomeIndicatorAutoHidden: Bool { true }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = UIColor(red: 0.09, green: 0.09, blue: 0.10, alpha: 1)

        metal.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(metal)
        let guide = view.safeAreaLayoutGuide
        NSLayoutConstraint.activate([
            metal.topAnchor.constraint(equalTo: guide.topAnchor),
            metal.bottomAnchor.constraint(equalTo: guide.bottomAnchor),
            metal.leadingAnchor.constraint(equalTo: guide.leadingAnchor),
            metal.trailingAnchor.constraint(equalTo: guide.trailingAnchor)
        ])

        metal.onOpenDialog = { [weak self] multiple, pictures in self?.showImportChooser(multiple: multiple, pictures: pictures) }
        metal.onSaveDialog = { [weak self] name in self?.showSaveSheet(name: name) }
        metal.onOpenURL = { url in
            guard let u = URL(string: url), let scheme = u.scheme?.lowercased(),
                  ["http", "https", "mailto"].contains(scheme) else { return }
            UIApplication.shared.open(u)
        }
        metal.onCopyText = { UIPasteboard.general.string = $0 }

        let nc = NotificationCenter.default
        nc.addObserver(forName: UIApplication.willResignActiveNotification, object: nil, queue: .main) { [weak self] _ in
            self?.metal.saveState()
            self?.metal.setFocused(false)
        }
        nc.addObserver(forName: UIApplication.didBecomeActiveNotification, object: nil, queue: .main) { [weak self] _ in
            self?.metal.setFocused(true)
        }
        nc.addObserver(forName: .pcAddFonts, object: nil, queue: .main) { [weak self] _ in
            self?.showFontPicker()
        }
        nc.addObserver(forName: .pcPhotosSaved, object: nil, queue: .main) { [weak self] note in
            let ok = (note.object as? Bool) ?? false
            if ok {
                self?.message("Saved to Photos", "The picture is now in your Photos app.")
            } else {
                let why = (note.userInfo?["error"] as? String) ?? ""
                self?.message("Couldn’t save to Photos", why.isEmpty ? "Allow PhotoCraft to add photos in Settings › PhotoCraft › Photos." : why)
            }
        }
        nc.addObserver(forName: .pcOpenURL, object: nil, queue: .main) { [weak self] note in
            guard let url = note.object as? URL else { return }
            self?.openFromOutside(url)
        }
    }

    // MARK: Fonts

    /// Apple's own font picker: the only way an app can use fonts installed on the iPad.
    private func showFontPicker() {
        guard presentedViewController == nil, view.window?.isKeyWindow ?? true else { return }
        var configuration = UIFontPickerViewController.Configuration()
        configuration.includeFaces = false
        let picker = UIFontPickerViewController(configuration: configuration)
        picker.delegate = self
        metal.suspendFocus = true
        present(picker, animated: true)
    }

    func fontPickerViewControllerDidPickFont(_ viewController: UIFontPickerViewController) {
        metal.suspendFocus = false
        guard let descriptor = viewController.selectedFontDescriptor else { return }
        let stored = SystemFonts.addPicked(descriptor)
        if presentedViewController != nil { dismiss(animated: true) }
        let family = (descriptor.object(forKey: .family) as? String) ?? "the font"
        if stored.isEmpty {
            message("Couldn’t add the font", "iPadOS did not let PhotoCraft copy “\(family)”. Some protected fonts can’t be used.")
        } else {
            message("Font added", "Added \(stored.count) style(s) of “\(family)”. Restart PhotoCraft once to use \(stored.count == 1 ? "it" : "them").")
        }
    }

    func fontPickerViewControllerDidCancel(_ viewController: UIFontPickerViewController) {
        metal.suspendFocus = false
    }

    // MARK: Importing: Photos or Files

    /// Asks where to import from (Photos or Files), then opens the matching Apple sheet.
    private func showImportChooser(multiple: Bool, pictures: Bool) {
        pcLog("import requested; several: \(multiple); pictures: \(pictures); already presenting: \(presentedViewController != nil)")
        guard presentedViewController == nil else {
            metal.deliverDialog(files: [], paths: [], ok: false)
            return
        }
        if !pictures {
            showFilesOpen(multiple: multiple)
            return
        }
        let sheet = UIAlertController(title: "Import from", message: nil, preferredStyle: .actionSheet)
        sheet.addAction(UIAlertAction(title: "Photos", style: .default) { [weak self] _ in self?.showPhotos(multiple: multiple) })
        sheet.addAction(UIAlertAction(title: "Files", style: .default) { [weak self] _ in self?.showFilesOpen(multiple: multiple) })
        sheet.addAction(UIAlertAction(title: "Cancel", style: .cancel) { [weak self] _ in
            self?.metal.suspendFocus = false
            self?.metal.deliverDialog(files: [], paths: [], ok: false)
        })
        if let pop = sheet.popoverPresentationController {
            pop.sourceView = view
            pop.sourceRect = CGRect(x: view.bounds.midX, y: view.bounds.midY, width: 1, height: 1)
            pop.permittedArrowDirections = []
        }
        metal.suspendFocus = true
        present(sheet, animated: true)
    }

    /// Apple's Open sheet. One file is opened where it is (no copy); several are copied in.
    private func showFilesOpen(multiple: Bool) {
        mode = .open
        openAsCopy = multiple
        let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.item], asCopy: multiple)
        picker.allowsMultipleSelection = multiple
        picker.delegate = self
        metal.suspendFocus = true
        present(picker, animated: true) { pcLog("open sheet is on screen") }
    }

    private func showPhotos(multiple: Bool) {
        var config = PHPickerConfiguration()
        config.filter = .images
        config.selectionLimit = multiple ? 0 : 1
        let picker = PHPickerViewController(configuration: config)
        picker.delegate = self
        metal.suspendFocus = true
        present(picker, animated: true)
    }

    func picker(_ picker: PHPickerViewController, didFinishPicking results: [PHPickerResult]) {
        picker.dismiss(animated: true)
        metal.suspendFocus = false
        if results.isEmpty {
            metal.deliverDialog(files: [], paths: [], ok: false)
            return
        }
        var found: [(Int, String, Data)] = []
        let lock = NSLock()
        let group = DispatchGroup()
        let keep = ["jpg", "jpeg", "png", "tif", "tiff", "webp", "gif"]
        for (i, result) in results.enumerated() {
            let provider = result.itemProvider
            group.enter()
            provider.loadDataRepresentation(forTypeIdentifier: UTType.image.identifier) { data, error in
                defer { group.leave() }
                guard let data = data else {
                    pcLog("photo \(i + 1) could not be read: \(error?.localizedDescription ?? "unknown")")
                    return
                }
                let type = provider.registeredTypeIdentifiers.first.flatMap { UTType($0) }
                var ext = type?.preferredFilenameExtension?.lowercased() ?? "jpg"
                var out = data
                if !keep.contains(ext) {
                    // HEIC and other Apple formats: PhotoCraft gets a lossless PNG.
                    if let image = UIImage(data: data), let png = image.pngData() {
                        out = png
                        ext = "png"
                    }
                }
                let base = provider.suggestedName ?? "Photo \(i + 1)"
                lock.lock()
                found.append((i, "\(base).\(ext)", out))
                lock.unlock()
            }
        }
        group.notify(queue: .main) { [weak self] in
            let files = found.sorted { $0.0 < $1.0 }.map { ($0.1, $0.2) }
            pcLog("photos: \(files.count) of \(results.count) read")
            self?.metal.deliverDialog(files: files, paths: [], ok: !files.isEmpty)
        }
    }

    /// A file opened from outside the app (Files > Open With, Share).
    private func openFromOutside(_ url: URL) {
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        do {
            let data = try Data(contentsOf: url)
            pcLog("opened from outside: \(url.lastPathComponent), \(data.count) bytes")
            metal.deliverOpened(name: url.lastPathComponent, data: data)
        } catch {
            pcLog("could not read \(url.lastPathComponent): \(error.localizedDescription)")
            message("Couldn’t open the file", error.localizedDescription)
        }
    }

    // MARK: Saving

    /// Pictures can go to Photos or Files; everything else goes to Files.
    private static let pictureTypes: Set<String> = ["png", "jpg", "jpeg", "tif", "tiff", "heic", "heif", "gif", "webp", "bmp"]

    private func showSaveSheet(name: String, attempt: Int = 0) {
        if presentedViewController != nil {
            if attempt < 8 {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in self?.showSaveSheet(name: name, attempt: attempt + 1) }
            } else {
                metal.deliverSaveDialog(path: nil)
            }
            return
        }
        let ext = URL(fileURLWithPath: name).pathExtension.lowercased()
        guard PhotoCraftViewController.pictureTypes.contains(ext) else {
            showFilesSaveSheet(name: name)
            return
        }
        let sheet = UIAlertController(title: "Save to", message: name, preferredStyle: .actionSheet)
        sheet.addAction(UIAlertAction(title: "Photos", style: .default) { [weak self] _ in self?.savePictureToPhotos(name: name) })
        sheet.addAction(UIAlertAction(title: "Files", style: .default) { [weak self] _ in self?.showFilesSaveSheet(name: name) })
        sheet.addAction(UIAlertAction(title: "Cancel", style: .cancel) { [weak self] _ in
            self?.metal.suspendFocus = false
            self?.metal.deliverSaveDialog(path: nil)
        })
        if let pop = sheet.popoverPresentationController {
            pop.sourceView = view
            pop.sourceRect = CGRect(x: view.bounds.midX, y: view.bounds.midY, width: 1, height: 1)
            pop.permittedArrowDirections = []
        }
        metal.suspendFocus = true
        present(sheet, animated: true)
    }

    /// PhotoCraft writes the picture into a temporary place; once it is written it is added to Photos.
    private func savePictureToPhotos(name: String) {
        metal.suspendFocus = false
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent("PhotoCraftPhotos-\(UUID().uuidString)", isDirectory: true)
        do {
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        } catch {
            pcLog("could not prepare the Photos export: \(error.localizedDescription)")
            metal.deliverSaveDialog(path: nil)
            return
        }
        let path = folder.appendingPathComponent(name).path
        pcLog("exporting to Photos via \(path)")
        PhotosExport.shared.expect(path)
        metal.deliverSaveDialog(path: path)
    }

    /// Apple's Save to Files sheet. The new file is created where you choose, and PhotoCraft
    /// then writes straight into it.
    private func showFilesSaveSheet(name: String) {
        pcLog("save sheet requested for \(name)")
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent("PhotoCraftSave-\(UUID().uuidString)", isDirectory: true)
        let file = folder.appendingPathComponent(name)
        do {
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            guard FileManager.default.createFile(atPath: file.path, contents: Data()) else { throw CocoaError(.fileWriteUnknown) }
        } catch {
            pcLog("could not prepare the save sheet: \(error.localizedDescription)")
            message("Couldn’t save", error.localizedDescription)
            metal.deliverSaveDialog(path: nil)
            return
        }
        savePlaceholder = file
        mode = .save
        let picker = UIDocumentPickerViewController(forExporting: [file], asCopy: false)
        picker.delegate = self
        metal.suspendFocus = true
        present(picker, animated: true)
    }

    private func cleanUpPlaceholder() {
        if let file = savePlaceholder {
            try? FileManager.default.removeItem(at: file.deletingLastPathComponent())
        }
        savePlaceholder = nil
    }

    // MARK: Picker results

    func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]) {
        pcLog("sheet returned \(urls.count) item(s) in mode \(mode)")
        metal.suspendFocus = false
        switch mode {
        case .open:
            if openAsCopy {
                var files: [(String, Data)] = []
                for url in urls {
                    let scoped = url.startAccessingSecurityScopedResource()
                    defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                    do {
                        let data = try Data(contentsOf: url)
                        pcLog("read \(data.count) bytes from \(url.lastPathComponent)")
                        files.append((url.lastPathComponent, data))
                    } catch {
                        pcLog("read failed: \(error.localizedDescription)")
                        message("Couldn’t open the file", error.localizedDescription)
                    }
                }
                metal.deliverDialog(files: files, paths: [], ok: !files.isEmpty)
            } else {
                var paths: [String] = []
                for url in urls {
                    FileAccess.shared.remember(url)
                    FileAccess.shared.prepareForReading(url)
                    paths.append(url.path)
                }
                metal.deliverDialog(files: [], paths: paths, ok: !paths.isEmpty)
            }
        case .save:
            if let url = urls.first {
                FileAccess.shared.remember(url)
                pcLog("save place chosen: \(url.path)")
                metal.deliverSaveDialog(path: url.path)
            } else {
                metal.deliverSaveDialog(path: nil)
            }
            cleanUpPlaceholder()
        }
    }

    func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
        pcLog("sheet cancelled in mode \(mode)")
        metal.suspendFocus = false
        switch mode {
        case .open:
            metal.deliverDialog(files: [], paths: [], ok: false)
        case .save:
            metal.deliverSaveDialog(path: nil)
            cleanUpPlaceholder()
        }
    }

    // MARK: Messages

    /// Shown a moment later, so it still appears while a picker is closing.
    private func message(_ title: String, _ text: String) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) { [weak self] in
            guard let self = self, self.presentedViewController == nil else { return }
            let a = UIAlertController(title: title, message: text, preferredStyle: .alert)
            a.addAction(UIAlertAction(title: "OK", style: .default))
            self.present(a, animated: true)
        }
    }
}
