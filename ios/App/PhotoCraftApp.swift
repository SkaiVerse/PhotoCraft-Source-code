import SwiftUI

extension Notification.Name {
    static let pcAddFonts = Notification.Name("PhotoCraftAddFonts")
    static let pcPhotosSaved = Notification.Name("PhotoCraftPhotosSaved")
    static let pcOpenURL = Notification.Name("PhotoCraftOpenURL")
}

@main
struct PhotoCraftApp: App {
    init() {
        // The app sizes its memory budgets from the machine's RAM, which iPadOS does not report to
        // it. Telling it about half of the real memory keeps big canvases under the system's limit.
        let halfMB = ProcessInfo.processInfo.physicalMemory / 2 / 1_048_576
        setenv("PHOTOCRAFT_RAM_MB", String(halfMB), 1)
        // Before any window exists, so PhotoCraft finds the iPad's fonts when it starts.
        SystemFonts.prepare()
        let info = Bundle.main.infoDictionary
        pcLog("memory budget based on \(halfMB) MB")
        pcLog("PhotoCraft version \(info?["CFBundleShortVersionString"] ?? "?") (build \(info?["CFBundleVersion"] ?? "?"))")
        // Reopen access to the files you used recently, so Recent files still open.
        FileAccess.shared.restore()
    }

    var body: some Scene {
        WindowGroup {
            ZStack {
                Color(red: 0.09, green: 0.09, blue: 0.10).ignoresSafeArea()
                PhotoCraftHost()
            }
            .statusBarHidden(true)
            .onOpenURL { url in
                NotificationCenter.default.post(name: .pcOpenURL, object: url)
            }
        }
        .commands {
            CommandMenu("Fonts") {
                Button("Add Installed Fonts…") {
                    NotificationCenter.default.post(name: .pcAddFonts, object: nil)
                }
                .keyboardShortcut("f", modifiers: [.command, .option, .shift])
            }
        }
    }
}

struct PhotoCraftHost: UIViewControllerRepresentable {
    func makeUIViewController(context: Context) -> PhotoCraftViewController { PhotoCraftViewController() }
    func updateUIViewController(_ vc: PhotoCraftViewController, context: Context) {}
}
