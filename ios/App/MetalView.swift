import UIKit
import QuartzCore

/// The drawing surface. Catches the Apple Pencil, fingers, the pointer, scrolling, pinching and
/// the keyboard, and hands them to the Rust core.
///
/// Palm rejection works like Procreate's: while the Pencil touches the screen or hovers over it
/// (and for a moment after), finger touches are ignored completely.
final class MetalView: UIView, UIKeyInput, UIGestureRecognizerDelegate, UIPencilInteractionDelegate, UIScribbleInteractionDelegate {

    // Stored state for the on-screen keyboard's text-cursor gestures (see VirtualTextInput.swift).
    weak var inputDelegate: UITextInputDelegate?
    var markedTextStyle: [NSAttributedString.Key: Any]?
    let vt = VirtualText()
    lazy var vtTokenizer: UITextInputTokenizer = UITextInputStringTokenizer(textInput: self)

    override class var layerClass: AnyClass { CAMetalLayer.self }
    var metalLayer: CAMetalLayer { layer as! CAMetalLayer }

    // Hooks the view controller fills in.
    var onOpenDialog: ((_ multiple: Bool, _ pictures: Bool) -> Void)?
    var onSaveDialog: ((String) -> Void)?
    var onOpenURL: ((String) -> Void)?
    var onCopyText: ((String) -> Void)?
    /// True while a picker or alert is showing, so the view doesn't grab the keyboard back.
    var suspendFocus = false

    var handle: UnsafeMutableRawPointer?
    private var startFailed = false
    private var link: CADisplayLink?
    private var softKeyboard = false
    var wantsKeyboard = false
    /// The person hid the on-screen keyboard while a text field is still active.
    private var keyboardDismissed = false
    private let emptyInput = UIView(frame: .zero)
    private var activeTouch: UITouch?
    private var pointerButton: Int32 = 0
    private var held: (hid: UInt32, mods: UInt32, text: String?, start: CFTimeInterval, last: CFTimeInterval)?

    // Interface size (1.0 is the normal size of the desktop app).
    private var uiScale: Float = 1.3

    // Apple Pencil and palm rejection.
    private var pencilTouch: UITouch?
    private var pencilHovering = false
    private var pencilLastSeen: CFTimeInterval = 0
    /// How long after the Pencil leaves that fingers are still ignored.
    private let palmGrace: CFTimeInterval = 0.45
    private var eraserOn = false
    private var lastHover = CGPoint(x: 200, y: 200)
    private var squeezePalette: SqueezePaletteView?
    private var hoverLogged = 0
    private var keyLogged = 0

    // Pencil samples wait a moment for iPadOS to deliver their true pressure.
    private struct PenItem {
        var phase: Int32
        var location: CGPoint
        var force: CGFloat
        var altitude: CGFloat
        var azimuth: CGVector
        var roll: CGFloat
        var mods: UInt32
        var index: NSNumber?
        var ready: Bool
        var time: CFTimeInterval
        var stamp: TimeInterval
    }
    private var penQueue: [PenItem] = []
    private var lastQueuedLocation: CGPoint?
    private var smoothedPressure: Float = 0

    // Fingers: one finger is decided on by the Rust side (tap, scroll, pan or draw);
    // two or more fingers are gestures.
    private var directTouches = Set<UITouch>()
    private var ignoredTouches = Set<UITouch>()
    private var fingerTouch: UITouch?
    private var fingerLast = CGPoint.zero
    /// Multi-touch tap arbitration: track peak simultaneous touches during gesture.
    private var maxTouchCount: Int = 0
    private var touchStartTime: TimeInterval = 0
    private var touchStartLocations: [ObjectIdentifier: CGPoint] = [:]
    private var multiTouchExceededSlop = false
    /// Transient buffer for single finger to avoid committing brush/pen dabs before multi-touch arbitration settles.
    private var pendingFingerTouch: UITouch?
    private var pendingFingerPos = CGPoint.zero
    private var pendingFingerMods: UInt32 = 0
    private var fingerDispatchTimer: Timer?
    /// A fingertip resting on the screen while the Pencil draws: holds Shift (keeps proportions).
    private var constrainTouch: UITouch?

    /// True while the Pencil is touching, hovering, or has only just left the screen.
    private var pencilNear: Bool {
        pruneTouches()
        let since = CACurrentMediaTime() - pencilLastSeen
        // A hover that stopped reporting counts for a second at most.
        return pencilTouch != nil || (pencilHovering && since < 1.0) || since < palmGrace
    }

    /// Forgets touches that have ended without being reported, so nothing ever gets stuck.
    private func pruneTouches() {
        if let t = pencilTouch, t.phase == .ended || t.phase == .cancelled { pencilTouch = nil }
        directTouches = directTouches.filter { $0.phase != .ended && $0.phase != .cancelled }
        ignoredTouches = ignoredTouches.filter { $0.phase != .ended && $0.phase != .cancelled }
        if let p = pendingFingerTouch, p.phase == .ended || p.phase == .cancelled {
            fingerDispatchTimer?.invalidate()
            fingerDispatchTimer = nil
            pendingFingerTouch = nil
        }
        if let f = fingerTouch, f.phase == .ended || f.phase == .cancelled { cancelFinger() }
        if let c = constrainTouch, c.phase == .ended || c.phase == .cancelled { releaseConstrain() }
    }

    // UITextInputTraits (no autocorrect while typing)
    var autocorrectionType: UITextAutocorrectionType = .no
    var autocapitalizationType: UITextAutocapitalizationType = .none
    var spellCheckingType: UITextSpellCheckingType = .no
    var smartQuotesType: UITextSmartQuotesType = .no
    var smartDashesType: UITextSmartDashesType = .no
    var smartInsertDeleteType: UITextSmartInsertDeleteType = .no
    var keyboardType: UIKeyboardType = .default

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        backgroundColor = UIColor(red: 0.09, green: 0.09, blue: 0.10, alpha: 1)
        inputAssistantItem.leadingBarButtonGroups = []
        inputAssistantItem.trailingBarButtonGroups = []
        setupGestures()
        let pencil = UIPencilInteraction()
        pencil.delegate = self
        addInteraction(pencil)
        let scribble = UIScribbleInteraction(delegate: self)
        addInteraction(scribble)
        registerForTraitChanges([UITraitUserInterfaceStyle.self]) { (view: MetalView, _) in
            view.pushTheme()
        }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    deinit {
        link?.invalidate()
        if let h = handle { pc_destroy(h) }
    }

    // MARK: Lifecycle

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window != nil {
            ensureHandle()
            if link == nil {
                let l = CADisplayLink(target: self, selector: #selector(tick))
                l.preferredFrameRateRange = CAFrameRateRange(minimum: 30, maximum: 120, preferred: 120)
                l.add(to: .main, forMode: .common)
                link = l
            }
        } else {
            link?.invalidate()
            link = nil
        }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        ensureHandle()
        if let h = handle {
            let s = traitCollection.displayScale
            pc_resize(h, UInt32(max(1, bounds.width * s)), UInt32(max(1, bounds.height * s)), Float(s))
        }
    }

    private func ensureHandle() {
        guard handle == nil, !startFailed, window != nil, bounds.width > 1, bounds.height > 1 else { return }
        let s = traitCollection.displayScale
        contentScaleFactor = s
        metalLayer.contentsScale = s
        let layerPtr = Unmanaged.passUnretained(metalLayer).toOpaque()
        handle = pc_create(layerPtr, UInt32(bounds.width * s), UInt32(bounds.height * s), Float(s), makeCallbacks())
        if handle == nil {
            startFailed = true
        } else {
            pushTheme()
            uiScale = autoScale()
            pc_set_ui_scale(handle, uiScale)
        }
    }

    /// Smaller iPads get a larger interface, so buttons are easy to hit with the Pencil.
    private func autoScale() -> Float {
        let size = window?.windowScene?.screen.bounds.size ?? CGSize(width: 834, height: 1194)
        let short = Float(min(size.width, size.height))
        return max(1.15, min(1.5, 1.15 + (1100 - short) / 1000))
    }

    private func pushTheme() {
        guard let h = handle else { return }
        pc_set_dark(h, traitCollection.userInterfaceStyle == .dark ? 1 : 0)
    }

    func saveState() { if let h = handle { pc_save_state(h) } }
    func setFocused(_ focused: Bool) { if let h = handle { pc_set_focused(h, focused ? 1 : 0) } }

    @objc private func tick() {
        guard let h = handle else { return }
        if !suspendFocus && !isFirstResponder { _ = becomeFirstResponder() }
        repeatHeldKey()
        pruneTouches()
        flushPen(all: false)
        let flags = pc_frame(h)
        let wants = (flags & 2) != 0
        if wants != wantsKeyboard {
            wantsKeyboard = wants
            if !wants { keyboardDismissed = false }
        }
        let show = wantsKeyboard && !keyboardDismissed
        if show != softKeyboard {
            softKeyboard = show
            reloadInputViews()
        }
    }

    // MARK: Callbacks from Rust

    fileprivate static func from(_ p: UnsafeMutableRawPointer?) -> MetalView? {
        p.map { Unmanaged<MetalView>.fromOpaque($0).takeUnretainedValue() }
    }

    private func makeCallbacks() -> PCCallbacks {
        let me = Unmanaged.passUnretained(self).toOpaque()
        return PCCallbacks(
            user: me,
            open_dialog: { u, multiple, pictures in
                guard let v = MetalView.from(u) else { return }
                DispatchQueue.main.async { v.onOpenDialog?(multiple != 0, pictures != 0) }
            },
            save_dialog: { u, name in
                guard let v = MetalView.from(u), let name = name else { return }
                let s = String(cString: name)
                DispatchQueue.main.async { v.onSaveDialog?(s) }
            },
            write_file: { _, path, data, len in
                guard let path = path, let data = data else { return 0 }
                let bytes = Data(bytes: data, count: len)
                let name = String(cString: path)
                let ok = FileAccess.shared.write(path: name, data: bytes)
                if ok { PhotosExport.shared.finished(path: name) }
                return ok ? 1 : 0
            },
            open_url: { u, url in
                guard let v = MetalView.from(u), let url = url else { return }
                let s = String(cString: url)
                DispatchQueue.main.async { v.onOpenURL?(s) }
            },
            copy_text: { u, text in
                guard let v = MetalView.from(u), let text = text else { return }
                let s = String(cString: text)
                DispatchQueue.main.async { v.onCopyText?(s) }
            }
        )
    }

    // MARK: Results from the view controller

    /// A file arrived from outside (Files, "Open in").
    func deliverOpened(name: String, data: Data) {
        guard let h = handle else { return }
        name.withCString { n in
            data.withUnsafeBytes { raw in
                pc_open_file(h, n, raw.bindMemory(to: UInt8.self).baseAddress, data.count)
            }
        }
    }

    /// The Open sheet finished: files copied in (Photos, several files), or files opened in place
    /// (their paths), or nothing when cancelled.
    func deliverDialog(files: [(String, Data)], paths: [String], ok: Bool) {
        guard let h = handle else { return }
        for path in paths { path.withCString { pc_dialog_path(h, $0) } }
        for (name, data) in files {
            name.withCString { n in
                data.withUnsafeBytes { raw in
                    pc_dialog_file(h, n, raw.bindMemory(to: UInt8.self).baseAddress, data.count)
                }
            }
        }
        pc_dialog_end(h, ok ? 1 : 0)
    }

    /// The Save to Files sheet finished: the place chosen, or nil when cancelled.
    func deliverSaveDialog(path: String?) {
        guard let h = handle else { return }
        if let path = path { path.withCString { pc_save_dialog_end(h, $0) } } else { pc_save_dialog_end(h, nil) }
    }

    // MARK: Keyboard

    override var canBecomeFirstResponder: Bool { true }
    override var inputView: UIView? { softKeyboard ? nil : emptyInput }

    /// The keyboard's own "hide keyboard" key lands here: keep it hidden until the next tap.
    override func resignFirstResponder() -> Bool {
        held = nil
        if softKeyboard {
            keyboardDismissed = true
            softKeyboard = false
        }
        return super.resignFirstResponder()
    }

    var hasText: Bool { true }

    func insertText(_ text: String) {
        guard let h = handle else { return }
        vt.marked = nil
        vt.advance(by: text == "\n" ? 0 : text.count)
        if text == "\n" {
            pc_key(h, 0x28, 1, 0, 0)
            pc_key(h, 0x28, 0, 0, 0)
        } else {
            text.withCString { pc_text(h, $0) }
        }
    }

    func deleteBackward() {
        guard let h = handle else { return }
        vt.advance(by: -1)
        pc_key(h, 0x2A, 1, 0, 0)
        pc_key(h, 0x2A, 0, 0, 0)
    }

    private func modBits(_ f: UIKeyModifierFlags) -> UInt32 {
        var m: UInt32 = 0
        if f.contains(.shift) { m |= 1 }
        if f.contains(.control) { m |= 2 }
        if f.contains(.alternate) { m |= 4 }
        if f.contains(.command) { m |= 8 }
        return m
    }

    /// Only real typing keys type text: letters, digits, punctuation, space and the keypad.
    /// Arrows, Return, Tab, Escape, F-keys and the like are key presses, never text.
    private func typesText(_ hid: UInt32) -> Bool {
        if hid >= 0x04 && hid <= 0x27 { return true }                 // letters and digits
        if hid >= 0x2C && hid <= 0x38 { return true }                 // space and punctuation
        if hid >= 0x54 && hid <= 0x63 && hid != 0x58 { return true }  // keypad (not its Enter)
        return false
    }

    private func printable(_ s: String) -> String? {
        guard !s.isEmpty else { return nil }
        let ok = s.unicodeScalars.allSatisfy { $0.value >= 0x20 && $0.value != 0x7F && !(0xF700...0xF8FF).contains($0.value) }
        return ok ? s : nil
    }

    /// ⌘C, ⌘X and ⌘V go through the iPad's clipboard.
    private func clipboardShortcut(hid: UInt32, mods: UInt32) -> Bool {
        guard mods == 8, let h = handle else { return false }
        switch hid {
        case 0x06: pc_copy(h); return true
        case 0x1B: pc_cut(h); return true
        case 0x19:
            let s = UIPasteboard.general.string ?? ""
            s.withCString { pc_paste(h, $0) }
            return true
        default: return false
        }
    }

    private func handlePress(_ press: UIPress, down: Bool) -> Bool {
        guard let key = press.key, let h = handle else { return false }
        let hid = UInt32(key.keyCode.rawValue)
        if hid >= 0xE0 && hid <= 0xE7 { return true }   // a modifier key on its own
        let mods = modBits(key.modifierFlags)
        if down && keyLogged < 20 {
            keyLogged += 1
            pcLog("key down: usage \(hid), modifiers \(mods)")
        }
        if down {
            if clipboardShortcut(hid: hid, mods: mods) { return true }
            pc_key(h, hid, 1, 0, mods)
            var text: String? = nil
            if typesText(hid) && mods & (8 | 2) == 0 { text = printable(key.characters) }
            if let t = text { t.withCString { pc_text(h, $0) } }
            let now = CACurrentMediaTime()
            held = (hid, mods, text, now, now)
        } else {
            pc_key(h, hid, 0, 0, mods)
            if held?.hid == hid { held = nil }
        }
        return true
    }

    private func repeatHeldKey() {
        guard let k = held, let h = handle else { return }
        let now = CACurrentMediaTime()
        if now - k.start > 0.4 && now - k.last > 0.035 {
            pc_key(h, k.hid, 1, 1, k.mods)
            if let t = k.text { t.withCString { pc_text(h, $0) } }
            held?.last = now
        }
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        var rest = Set<UIPress>()
        for p in presses where !handlePress(p, down: true) { rest.insert(p) }
        if !rest.isEmpty { super.pressesBegan(rest, with: event) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        var rest = Set<UIPress>()
        for p in presses where !handlePress(p, down: false) { rest.insert(p) }
        if !rest.isEmpty { super.pressesEnded(rest, with: event) }
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        held = nil
        super.pressesCancelled(presses, with: event)
    }

    // ⌘ shortcuts are claimed ahead of the system so ⌘Z, ⌘S, ⌘J and friends reach the app.
    private static let commandKeys: [UIKeyCommand] = {
        let letters = Array("abcdefgijklmnoprstuvxyz0123456789-=[];',./\\`").map { String($0) }
        let specials = [UIKeyCommand.inputUpArrow, UIKeyCommand.inputDownArrow, UIKeyCommand.inputLeftArrow,
                        UIKeyCommand.inputRightArrow, UIKeyCommand.inputPageUp, UIKeyCommand.inputPageDown,
                        UIKeyCommand.inputHome, UIKeyCommand.inputEnd]
        let mods: [UIKeyModifierFlags] = [.command, [.command, .shift], [.command, .alternate], [.command, .shift, .alternate]]
        var list: [UIKeyCommand] = []
        for input in letters + specials {
            for m in mods {
                let c = UIKeyCommand(input: input, modifierFlags: m, action: #selector(MetalView.commandKey(_:)))
                c.wantsPriorityOverSystemBehavior = true
                list.append(c)
            }
        }
        return list
    }()

    override var keyCommands: [UIKeyCommand]? { MetalView.commandKeys }

    private static func hid(for s: String) -> UInt32? {
        switch s {
        case UIKeyCommand.inputUpArrow: return 0x52
        case UIKeyCommand.inputDownArrow: return 0x51
        case UIKeyCommand.inputLeftArrow: return 0x50
        case UIKeyCommand.inputRightArrow: return 0x4F
        case UIKeyCommand.inputPageUp: return 0x4B
        case UIKeyCommand.inputPageDown: return 0x4E
        case UIKeyCommand.inputHome: return 0x4A
        case UIKeyCommand.inputEnd: return 0x4D
        default: break
        }
        guard s.unicodeScalars.count == 1, let u = s.lowercased().unicodeScalars.first else { return nil }
        switch u {
        case "a"..."z": return 0x04 + (u.value - 97)
        case "1"..."9": return 0x1E + (u.value - 49)
        case "0": return 0x27
        case "-": return 0x2D
        case "=": return 0x2E
        case "[": return 0x2F
        case "]": return 0x30
        case "\\": return 0x31
        case ";": return 0x33
        case "'": return 0x34
        case "`": return 0x35
        case ",": return 0x36
        case ".": return 0x37
        case "/": return 0x38
        default: return nil
        }
    }

    @objc func commandKey(_ cmd: UIKeyCommand) {
        guard let h = handle, let input = cmd.input, let hid = MetalView.hid(for: input) else { return }
        let mods = modBits(cmd.modifierFlags)
        if clipboardShortcut(hid: hid, mods: mods) { return }
        pc_key(h, hid, 1, 0, mods)
        pc_key(h, hid, 0, 0, mods)
    }

    /// A key press and release, like a shortcut typed on a keyboard.
    func tapKey(_ hid: UInt32, mods: UInt32 = 0) {
        guard let h = handle else { return }
        pc_key(h, hid, 1, 0, mods)
        pc_key(h, hid, 0, 0, mods)
    }

    // MARK: Apple Pencil buttons

    /// Double tap on the Pencil: switch between the Brush (B) and the Eraser (E).
    func pencilInteraction(_ interaction: UIPencilInteraction, didReceiveTap tap: UIPencilInteraction.Tap) {
        pcLog("pencil: double tap")
        eraserOn.toggle()
        tapKey(eraserOn ? 0x08 : 0x05)
    }

    /// Squeeze on the Pencil Pro: opens the floating arc tool menu (or toggles brush options if active tool is brush-like).
    func pencilInteraction(_ interaction: UIPencilInteraction, didReceiveSqueeze squeeze: UIPencilInteraction.Squeeze) {
        guard squeeze.phase == .ended, let h = handle else { return }
        pcLog("pencil: squeeze")
        if let existing = squeezePalette {
            existing.dismiss()
            squeezePalette = nil
            pc_close_brush_picker(h)
            return
        }
        let loc = (lastHover != .zero) ? lastHover : CGPoint(x: bounds.midX, y: bounds.midY)
        let optionsLoc = CGPoint(x: loc.x, y: max(80, loc.y - 120))
        let brushMenuResult = pc_show_brush_options(h, Float(optionsLoc.x), Float(optionsLoc.y))
        if brushMenuResult != 0 {
            // Active tool is a brush/pencil/eraser/etc. - toggled brush options menu, do NOT open arc menu.
            return
        }
        let palette = SqueezePaletteView(centerAt: loc, in: self) { [weak self] toolId in
            guard let self = self, let h = self.handle else { return }
            toolId.withCString { pc_select_tool(h, $0) }
            self.squeezePalette = nil
        } onDismiss: { [weak self] in
            guard let self = self else { return }
            if let h = self.handle {
                pc_close_brush_picker(h)
            }
            self.squeezePalette = nil
        }
        squeezePalette = palette
        addSubview(palette)
        palette.animateIn()
    }

    // MARK: Apple Pencil Scribble
    func scribbleInteraction(_ interaction: UIScribbleInteraction, shouldBeginAt location: CGPoint) -> Bool {
        false
    }

    // MARK: Gestures

    private func setupGestures() {
        let direct = [NSNumber(value: UITouch.TouchType.direct.rawValue)]

        let hover = UIHoverGestureRecognizer(target: self, action: #selector(hoverMoved(_:)))
        hover.delegate = self
        addGestureRecognizer(hover)

        let wheel = UIPanGestureRecognizer(target: self, action: #selector(pointerScroll(_:)))
        wheel.allowedScrollTypesMask = .all
        wheel.allowedTouchTypes = []
        wheel.cancelsTouchesInView = false
        wheel.delegate = self
        addGestureRecognizer(wheel)

        let twoFinger = UIPanGestureRecognizer(target: self, action: #selector(twoFingerScroll(_:)))
        twoFinger.minimumNumberOfTouches = 2
        twoFinger.maximumNumberOfTouches = 2
        twoFinger.allowedTouchTypes = direct
        twoFinger.cancelsTouchesInView = false
        twoFinger.delegate = self
        addGestureRecognizer(twoFinger)

        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(pinched(_:)))
        pinch.allowedTouchTypes = direct
        pinch.cancelsTouchesInView = false
        pinch.delegate = self
        addGestureRecognizer(pinch)

        let twist = UIRotationGestureRecognizer(target: self, action: #selector(twisted(_:)))
        twist.allowedTouchTypes = direct
        twist.cancelsTouchesInView = false
        twist.delegate = self
        twistRecognizer = twist
        addGestureRecognizer(twist)

        // Press-and-hold on canvas opens context menu (finger and Apple Pencil)
        let longPress = UILongPressGestureRecognizer(target: self, action: #selector(canvasLongPress(_:)))
        longPress.minimumPressDuration = 0.45
        longPress.allowableMovement = 12
        longPress.allowedTouchTypes = [
            NSNumber(value: UITouch.TouchType.direct.rawValue),
            NSNumber(value: UITouch.TouchType.pencil.rawValue)
        ]
        longPress.cancelsTouchesInView = false
        longPress.delegate = self
        addGestureRecognizer(longPress)

        // Note: Two-finger Undo and three-finger Redo are tracked directly in the touch-routing state machine
        // (finish / touchesEnded) using maxTouchCount to prevent 2-finger Undo from firing during 3-finger Redo.
    }

    func gestureRecognizer(_ g: UIGestureRecognizer, shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool { true }

    /// Palm rejection: no finger gesture starts while the Pencil is near.
    func gestureRecognizer(_ g: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
        // Hovering is how the Pencil is noticed before it touches: never filter it out.
        if g is UIHoverGestureRecognizer { return true }
        if g is UILongPressGestureRecognizer {
            let p = touch.location(in: self)
            // Long press context menu is strictly for canvas; never intercept panel/dock/toolbar touches
            let onCanvas = p.x > 80 && p.x < (bounds.width - 320) && p.y > 50
            return onCanvas && !(touch.type == .direct && pencilNear)
        }
        if touch.type == .pencil { return false }
        if touch.type == .direct && (pencilNear || constrainTouch != nil || touch === constrainTouch) { return false }
        return true
    }

    @objc private func canvasLongPress(_ g: UILongPressGestureRecognizer) {
        guard g.state == .began, let h = handle else { return }
        let p = g.location(in: self)
        pcLog("canvas: long press at \(p)")
        pc_long_press(h, Float(p.x), Float(p.y))
    }

    @objc private func hoverMoved(_ g: UIHoverGestureRecognizer) {
        guard let h = handle else { return }
        // An Apple Pencil hovering reports a distance from the glass; a mouse or trackpad does not.
        let isPencil = g.zOffset > 0
        if hoverLogged < 8 {
            hoverLogged += 1
            pcLog("hover: state \(g.state.rawValue), pencil: \(isPencil), zOffset \(g.zOffset)")
        }
        switch g.state {
        case .began, .changed:
            let p = g.location(in: self)
            lastHover = p
            if isPencil {
                pencilHovering = true
                pencilLastSeen = CACurrentMediaTime()
                if fingerTouch != nil { cancelFinger() }
                let alt = Double(g.altitudeAngle)
                let az = g.azimuthUnitVector(in: self)
                let (tx, ty) = self.tilt(altitude: alt, azimuth: az)
                var roll: Float = 0
                if #available(iOS 17.5, *) { roll = Float(g.rollAngle * 180 / CGFloat.pi) }
                pc_pen_hover(h, Float(p.x), Float(p.y), tx, ty, roll)
            } else {
                pc_pointer_moved(h, Float(p.x), Float(p.y))
            }
        case .ended, .cancelled:
            if pencilHovering {
                pencilHovering = false
                pencilLastSeen = CACurrentMediaTime()
                pc_pen_hover_end(h)
            } else {
                pc_pointer_gone(h)
            }
        default: break
        }
    }

    /// Tilt in degrees on both axes (like a browser's pen events), from the Pencil's altitude and azimuth,
    /// transformed by canvas rotation (phi_canvas = phi_screen - theta_canvas) so brush tilt aligns with artwork.
    private func tilt(altitude: Double, azimuth: CGVector) -> (Float, Float) {
        var az = azimuth
        if let h = handle {
            let canvasRotDeg = Double(pc_canvas_rotation(h))
            if canvasRotDeg != 0.0 {
                let rotRad = canvasRotDeg * Double.pi / 180.0
                let phiScreen = atan2(Double(azimuth.dy), Double(azimuth.dx))
                let phiCanvas = phiScreen - rotRad
                az = CGVector(dx: cos(phiCanvas), dy: sin(phiCanvas))
            }
        }
        let upright = max(sin(altitude), 0.0001)
        let flat = cos(altitude)
        let tx = Float(atan2(Double(az.dx) * flat, upright) * 180 / Double.pi)
        let ty = Float(atan2(Double(az.dy) * flat, upright) * 180 / Double.pi)
        return (tx, ty)
    }

    @objc private func pointerScroll(_ g: UIPanGestureRecognizer) {
        guard let h = handle, g.state != .ended, g.state != .cancelled, g.state != .failed else { return }
        let t = g.translation(in: self)
        g.setTranslation(.zero, in: self)
        if t != .zero { pc_scroll(h, Float(t.x), Float(t.y), modBits(g.modifierFlags)) }
    }

    private var pinchTotal: CGFloat = 1
    private var twistRecognizer: UIRotationGestureRecognizer?
    private var twistEngaged = false
    private var twistSent: CGFloat = 0

    @objc private func twoFingerScroll(_ g: UIPanGestureRecognizer) {
        guard let h = handle else { return }
        if g.state == .began {
            cancelFinger()
        }
        if g.state == .ended || g.state == .cancelled {
            pc_pointer_gone(h)
            return
        }
        guard g.state != .failed else { return }
        let t = g.translation(in: self)
        g.setTranslation(.zero, in: self)
        let c = g.location(in: self)
        pc_pointer_moved(h, Float(c.x), Float(c.y))
        if t != .zero {
            pc_scroll(h, Float(t.x), Float(t.y), 0)
        }
    }

    /// Two-finger twist turns the canvas view only. It starts after a small twist (fluid like Affinity),
    /// allowing simultaneous two-finger pan, pinch zoom, and rotation.
    @objc private func twisted(_ g: UIRotationGestureRecognizer) {
        guard let h = handle else { return }
        switch g.state {
        case .began:
            cancelFinger()
            twistEngaged = false
            twistSent = 0
        case .changed:
            let total = g.rotation   // radians since the twist began, clockwise is positive
            if !twistEngaged {
                let degrees = abs(total) * 180 / CGFloat.pi
                if degrees < 1.0 { return }
                twistEngaged = true
                twistSent = total
                return
            }
            let delta = total - twistSent
            twistSent = total
            // Always rotate around the canvas center.
            let cx = Float(bounds.midX)
            let cy = Float(bounds.midY)
            pc_rotate(h, Float(delta), cx, cy)
        case .ended, .cancelled:
            twistEngaged = false
        default: break
        }
    }

    @objc private func pinched(_ g: UIPinchGestureRecognizer) {
        guard let h = handle else { return }
        switch g.state {
        case .began:
            cancelFinger()
            pinchTotal = 1
        case .changed:
            let p = g.location(in: self)
            pinchTotal *= g.scale
            pc_zoom(h, Float(g.scale), Float(p.x), Float(p.y))
            g.scale = 1
        case .ended, .cancelled:
            pc_pointer_gone(h)
        default: break
        }
    }

    @objc private func twoFingerTap(_ g: UITapGestureRecognizer) {
        guard g.state == .ended, !pencilNear else { return }
        tapKey(0x1D, mods: 8)        // ⌘Z
    }

    @objc private func threeFingerTap(_ g: UITapGestureRecognizer) {
        guard g.state == .ended, !pencilNear else { return }
        tapKey(0x1D, mods: 8 | 1)    // ⇧⌘Z
    }

    // MARK: Pencil, pointer and finger touches

    private func releaseConstrain() {
        constrainTouch = nil
        if let h = handle { pc_set_touch_shift(h, 0) }
    }

    private func dispatchPendingFinger() {
        guard let t = pendingFingerTouch, let h = handle else { return }
        pendingFingerTouch = nil
        fingerDispatchTimer = nil
        if directTouches.count == 1 && maxTouchCount == 1 {
            fingerTouch = t
            fingerLast = pendingFingerPos
            pc_finger(h, 0, Float(pendingFingerPos.x), Float(pendingFingerPos.y), pendingFingerMods)
        }
    }

    /// Drops a finger that was acting as the pointer (a second finger or the Pencil arrived).
    private func cancelFinger() {
        fingerDispatchTimer?.invalidate()
        fingerDispatchTimer = nil
        pendingFingerTouch = nil
        if fingerTouch != nil, let h = handle {
            pc_finger(h, 3, Float(fingerLast.x), Float(fingerLast.y), 0)
        }
        fingerTouch = nil
    }

    /// Adds a Pencil sample. It is sent immediately without artificial delay.
    private func queuePen(_ touch: UITouch, phase: Int32, event: UIEvent?, mods: UInt32) {
        if phase == 0 {
            lastQueuedLocation = touch.preciseLocation(in: self)
        }
        var samples: [UITouch] = [touch]
        if phase == 1, let more = event?.coalescedTouches(for: touch), !more.isEmpty { samples = more }
        let zoom = handle != nil ? Double(pc_canvas_zoom(handle)) : 1.0
        // Canvas-space aware sample rejection: 0.5 pt at 1.0x, scaling down with zoom to preserve sub-pixel detail
        let minScreenDist = min(0.5, 0.5 * max(zoom, 0.01))
        for c in samples {
            let loc = c.preciseLocation(in: self)
            if phase == 1, let prev = lastQueuedLocation {
                let dx = loc.x - prev.x
                let dy = loc.y - prev.y
                if (dx * dx + dy * dy) < (minScreenDist * minScreenDist) {
                    continue
                }
            }
            if phase == 1 {
                lastQueuedLocation = loc
            }
            let maxForce = c.maximumPossibleForce
            let normForce = max(0.0009765625, min(1.0, maxForce > 0 ? (c.force / maxForce) : 0.5))
            let waiting = c.estimationUpdateIndex != nil && c.estimatedPropertiesExpectingUpdates.contains(.force)
            var roll: CGFloat = 0
            if #available(iOS 17.5, *) { roll = c.rollAngle }
            penQueue.append(PenItem(
                phase: phase,
                location: loc,
                force: normForce,
                altitude: c.altitudeAngle,
                azimuth: c.azimuthUnitVector(in: self),
                roll: roll,
                mods: mods,
                index: waiting ? c.estimationUpdateIndex : nil,
                ready: !waiting,
                time: CACurrentMediaTime(),
                stamp: c.timestamp))
        }
        if phase != 1 {
            lastQueuedLocation = nil
        }
        flushPen(all: true)
    }

    /// iPadOS refines the Pencil's pressure a few milliseconds after the touch. Matching the
    /// refined value to its sample gives an even, natural stroke.
    override func touchesEstimatedPropertiesUpdated(_ touches: Set<UITouch>) {
        guard let h = handle else { return }
        for t in touches where t.type == .pencil {
            let maxForce = t.maximumPossibleForce
            let normForce = max(0.0009765625, min(1.0, maxForce > 0 ? (t.force / maxForce) : 0.5))
            if let index = t.estimationUpdateIndex,
               let i = penQueue.firstIndex(where: { $0.index == index }) {
                penQueue[i].force = normForce
                penQueue[i].altitude = t.altitudeAngle
                if !t.estimatedPropertiesExpectingUpdates.contains(.force) { penQueue[i].ready = true }
            }
            if t === pencilTouch {
                let (tx, ty) = self.tilt(altitude: t.altitudeAngle, azimuth: t.azimuthUnitVector(in: self))
                var roll: Float = 0
                if #available(iOS 17.5, *) { roll = Float(t.rollAngle * 180 / CGFloat.pi) }
                pc_pen_update_estimate(h, Float(normForce), tx, ty, roll)
            }
        }
        flushPen(all: true)
    }

    private func flushPen(all: Bool) {
        let now = CACurrentMediaTime()
        while let first = penQueue.first, first.ready || all || now - first.time > 0.004 {
            penQueue.removeFirst()
            sendPen(first)
        }
    }

    // MARK: Smooth curve through the Pencil samples

    private struct PenPoint {
        var x: Double
        var y: Double
        var pressure: Float
        var tiltX: Float
        var tiltY: Float
        var roll: Float
    }

    /// The last few real samples. Each new sample lets the curve between the two before it be drawn smoothly.
    private var fit: [PenPoint] = []
    private var statSum = 0.0
    private var statMax = 0.0
    private var statCount = 0

    private func sendPen(_ item: PenItem) {
        guard handle != nil else { return }
        // The Pencil's normalized force (0.0009765625 to 1.0)
        var pressure = Float(max(0.0009765625, min(1.0, item.force)))
        // Responsive exponential moving average (alpha = 0.4)
        if item.phase == 0 { smoothedPressure = pressure } else { smoothedPressure = smoothedPressure * 0.6 + pressure * 0.4 }
        pressure = max(0.0009765625, min(1.0, smoothedPressure))
        let (tx, ty) = self.tilt(altitude: Double(item.altitude), azimuth: item.azimuth)
        let point = PenPoint(x: Double(item.location.x), y: Double(item.location.y), pressure: pressure, tiltX: tx, tiltY: ty,
                             roll: Float(item.roll * 180 / CGFloat.pi))

        // How late the sample reaches the app, measured from the moment of the touch.
        let delay = (ProcessInfo.processInfo.systemUptime - item.stamp) * 1000
        statSum += delay
        statMax = max(statMax, delay)
        statCount += 1
        if statCount >= 240 {
            pcLog(String(format: "pencil: shell delay average %.1f ms, worst %.1f ms (240 samples)", statSum / Double(statCount), statMax))
            statSum = 0; statMax = 0; statCount = 0
        }

        switch item.phase {
        case 0:
            fit = [point]
            emitPen(point, phase: 0, mods: item.mods)
        case 1:
            fit.append(point)
            if fit.count > 4 { fit.removeFirst() }
            // With a new sample in hand, the curve between the two samples before it is final.
            if fit.count >= 3 {
                let n = fit.count
                let p1 = fit[n - 3], p2 = fit[n - 2]
                let p0 = n >= 4 ? fit[n - 4] : nil
                emitSegment(p0, p1, p2, fit[n - 1], mods: item.mods)
                emitPen(p2, phase: 1, mods: item.mods)
            }
        default:
            // The stroke ends: finish the curve up to the last sample, then lift.
            fit.append(point)
            if fit.count > 4 { fit.removeFirst() }
            let n = fit.count
            if n >= 3 {
                let p0 = n >= 4 ? fit[n - 4] : nil
                emitSegment(p0, fit[n - 3], fit[n - 2], fit[n - 1], mods: item.mods)
                emitPen(fit[n - 2], phase: 1, mods: item.mods)
            }
            if n >= 2 {
                let p0 = n >= 3 ? fit[n - 3] : nil
                emitSegment(p0, fit[n - 2], fit[n - 1], nil, mods: item.mods)
            }
            emitPen(point, phase: item.phase, mods: item.mods)
            fit.removeAll()
        }
    }

    private func emitPen(_ p: PenPoint, phase: Int32, mods: UInt32) {
        guard let h = handle else { return }
        pc_pen(h, Float(p.x), Float(p.y), phase, p.pressure, p.tiltX, p.tiltY, p.roll, mods)
    }

    /// Extra points along a smooth (centripetal Catmull-Rom) curve from `p1` to `p2`, so a fast
    /// stroke is drawn as a curve rather than as straight chords. `p0` and `p3` are the
    /// neighbouring samples; a missing neighbour uses one-sided forward/backward tangents.
    private func emitSegment(_ p0: PenPoint?, _ p1: PenPoint, _ p2: PenPoint, _ p3: PenPoint?, mods: UInt32) {
        let dx = p2.x - p1.x, dy = p2.y - p1.y
        let length = (dx * dx + dy * dy).squareRoot()
        let zoom = handle != nil ? Double(pc_canvas_zoom(handle)) : 1.0
        let canvasLength = length / max(zoom, 0.01)
        if length < 1.0 || canvasLength < 0.5 { return }
        let stepScreen = 2.0
        let count = min(24, max(1, Int((length / stepScreen).rounded(.up)))) - 1
        if count < 1 { return }

        // Centripetal Catmull-Rom (alpha = 0.5: Euclidean distance to power 0.5)
        func dist(_ u: PenPoint, _ v: PenPoint) -> Double {
            max(((u.x - v.x) * (u.x - v.x) + (u.y - v.y) * (u.y - v.y)).squareRoot().squareRoot(), 0.7071)
        }
        let dt12 = dist(p1, p2)

        // One-sided end tangents: when p0 is missing or too close to p1 (< 0.5 pt), tangent at p1 is strictly
        // the forward difference (p2 - p1)/dt12; when p3 is missing or too close to p2 (< 0.5 pt), tangent at p2
        // is strictly the backward difference (p2 - p1)/dt12. This eliminates division-by-zero tangent spikes
        // and wild overshoots from duplicate or near-duplicate points.
        let len01 = p0 != nil ? ((p0!.x - p1.x) * (p0!.x - p1.x) + (p0!.y - p1.y) * (p0!.y - p1.y)).squareRoot() : 0.0
        let a = (p0 != nil && len01 >= 0.5) ? p0! : PenPoint(x: 2 * p1.x - p2.x, y: 2 * p1.y - p2.y, pressure: p1.pressure, tiltX: p1.tiltX, tiltY: p1.tiltY, roll: p1.roll)
        let dt01 = (p0 != nil && len01 >= 0.5) ? dist(a, p1) : dt12

        let len23 = p3 != nil ? ((p3!.x - p2.x) * (p3!.x - p2.x) + (p3!.y - p2.y) * (p3!.y - p2.y)).squareRoot() : 0.0
        let d = (p3 != nil && len23 >= 0.5) ? p3! : PenPoint(x: 2 * p2.x - p1.x, y: 2 * p2.y - p1.y, pressure: p2.pressure, tiltX: p2.tiltX, tiltY: p2.tiltY, roll: p2.roll)
        let dt23 = (p3 != nil && len23 >= 0.5) ? dist(p2, d) : dt12

        let t0 = 0.0
        let t1 = t0 + dt01
        let t2 = t1 + dt12
        let t3 = t2 + dt23

        func mix(_ u: (Double, Double), _ v: (Double, Double), _ w: Double) -> (Double, Double) {
            (u.0 * (1 - w) + v.0 * w, u.1 * (1 - w) + v.1 * w)
        }
        var rollDelta = p2.roll - p1.roll
        if rollDelta > 180 { rollDelta -= 360 } else if rollDelta < -180 { rollDelta += 360 }

        let chordMidX = (p1.x + p2.x) * 0.5
        let chordMidY = (p1.y + p2.y) * 0.5
        let maxDevSq = (length * 1.5 + 8.0) * (length * 1.5 + 8.0)

        for i in 1...count {
            let u = Double(i) / Double(count + 1)
            let t = t1 + (t2 - t1) * u
            let A1 = mix((a.x, a.y), (p1.x, p1.y), (t - t0) / (t1 - t0))
            let A2 = mix((p1.x, p1.y), (p2.x, p2.y), (t - t1) / (t2 - t1))
            let A3 = mix((p2.x, p2.y), (d.x, d.y), (t - t2) / (t3 - t2))
            let B1 = mix(A1, A2, (t - t0) / (t2 - t0))
            let B2 = mix(A2, A3, (t - t1) / (t3 - t1))
            let C = mix(B1, B2, (t - t1) / (t2 - t1))
            if !C.0.isFinite || !C.1.isFinite { continue }
            let devX = C.0 - chordMidX
            let devY = C.1 - chordMidY
            if (devX * devX + devY * devY) > maxDevSq { continue }

            let f = Float(u)
            let midPressure = max(0.0009765625, min(1.0, p1.pressure + (p2.pressure - p1.pressure) * f))
            let mid = PenPoint(x: C.0, y: C.1,
                               pressure: midPressure,
                               tiltX: p1.tiltX + (p2.tiltX - p1.tiltX) * f,
                               tiltY: p1.tiltY + (p2.tiltY - p1.tiltY) * f,
                               roll: p1.roll + rollDelta * f)
            emitPen(mid, phase: 1, mods: mods)
        }
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let h = handle else { return }
        pruneTouches()
        keyboardDismissed = false
        let mods = modBits(event?.modifierFlags ?? [])
        if directTouches.isEmpty {
            maxTouchCount = 0
            touchStartTime = CACurrentMediaTime()
            multiTouchExceededSlop = false
            touchStartLocations.removeAll()
        }
        for t in touches {
            let p = t.location(in: self)
            switch t.type {
            case .pencil:
                // Anything already resting on the screen is a palm.
                for d in directTouches { ignoredTouches.insert(d) }
                directTouches.removeAll()
                cancelFinger()
                pencilTouch = t
                pencilLastSeen = CACurrentMediaTime()
                penQueue.removeAll()
                queuePen(t, phase: 0, event: event, mods: mods)
            case .indirectPointer:
                pointerButton = (event?.buttonMask.contains(.secondary) ?? false) ? 1 : 0
                pc_pointer_moved(h, Float(p.x), Float(p.y))
                pc_pointer_button(h, Float(p.x), Float(p.y), pointerButton, 1, mods)
            default:
                // One fingertip on the screen while the Pencil or primary finger is drawing,
                // creating a shape, or transforming: keeps proportions (Shift modifier).
                if (pencilTouch != nil || fingerTouch != nil) && constrainTouch == nil && t !== fingerTouch && t.majorRadius < 32 {
                    constrainTouch = t
                    pc_set_touch_shift(h, 1)
                    continue
                }
                if pencilNear {
                    ignoredTouches.insert(t)
                    continue
                }
                directTouches.insert(t)
                touchStartLocations[ObjectIdentifier(t)] = p
                maxTouchCount = max(maxTouchCount, directTouches.count)
                if maxTouchCount >= 2 {
                    // Multi-touch gesture (2-finger undo/pan/zoom/rotate or 3-finger redo):
                    // Discard any pending or active single-finger touch immediately so zero pixels are stamped!
                    cancelFinger()
                } else if directTouches.count == 1 {
                    // Buffer initial single-finger touch briefly (45ms) to arbitrate against a 2nd or 3rd finger landing.
                    pendingFingerTouch = t
                    pendingFingerPos = p
                    pendingFingerMods = mods
                    fingerDispatchTimer?.invalidate()
                    fingerDispatchTimer = Timer.scheduledTimer(withTimeInterval: 0.045, repeats: false) { [weak self] _ in
                        self?.dispatchPendingFinger()
                    }
                }
            }
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let h = handle else { return }
        let mods = modBits(event?.modifierFlags ?? [])
        for t in touches {
            let p = t.location(in: self)
            switch t.type {
            case .pencil:
                if t === pencilTouch {
                    queuePen(t, phase: 1, event: event, mods: mods)
                    if let predicted = event?.predictedTouches(for: t), !predicted.isEmpty {
                        for pt in predicted {
                            let maxForce = pt.maximumPossibleForce
                            let normForce = max(0.0009765625, min(1.0, maxForce > 0 ? (pt.force / maxForce) : 0.5))
                            let loc = pt.preciseLocation(in: self)
                            let (tx, ty) = self.tilt(altitude: pt.altitudeAngle, azimuth: pt.azimuthUnitVector(in: self))
                            var roll: Float = 0
                            if #available(iOS 17.5, *) { roll = Float(pt.rollAngle * 180 / CGFloat.pi) }
                            pc_pen_predicted(h, Float(loc.x), Float(loc.y), Float(normForce), tx, ty, roll)
                        }
                    } else {
                        pc_pen_predicted_clear(h)
                    }
                }
            case .indirectPointer:
                pc_pointer_moved(h, Float(p.x), Float(p.y))
            default:
                if let start = touchStartLocations[ObjectIdentifier(t)] {
                    let dx = p.x - start.x, dy = p.y - start.y
                    if (dx * dx + dy * dy) > (15.0 * 15.0) {
                        multiTouchExceededSlop = true
                    }
                }
                if t === pendingFingerTouch {
                    let dx = p.x - pendingFingerPos.x, dy = p.y - pendingFingerPos.y
                    if (dx * dx + dy * dy) > (8.0 * 8.0) {
                        // Dragging deliberately: commit single finger now and forward move
                        fingerDispatchTimer?.invalidate()
                        fingerDispatchTimer = nil
                        dispatchPendingFinger()
                        if fingerTouch != nil {
                            fingerLast = p
                            pc_finger(h, 1, Float(p.x), Float(p.y), mods)
                        }
                    }
                    continue
                }
                guard t === fingerTouch else { continue }
                fingerLast = p
                pc_finger(h, 1, Float(p.x), Float(p.y), mods)
            }
        }
    }

    private func finish(_ touches: Set<UITouch>, with event: UIEvent?, cancelled: Bool) {
        guard let h = handle else { return }
        let mods = modBits(event?.modifierFlags ?? [])
        for t in touches {
            let p = t.location(in: self)
            ignoredTouches.remove(t)
            switch t.type {
            case .pencil:
                if t === pencilTouch {
                    pc_pen_predicted_clear(h)
                    queuePen(t, phase: cancelled ? 3 : 2, event: event, mods: mods)
                    pencilTouch = nil
                    pencilLastSeen = CACurrentMediaTime()
                }
            case .indirectPointer:
                pc_pointer_button(h, Float(p.x), Float(p.y), pointerButton, 0, mods)
            default:
                if t === constrainTouch {
                    releaseConstrain()
                    continue
                }
                if t === pendingFingerTouch {
                    fingerDispatchTimer?.invalidate()
                    fingerDispatchTimer = nil
                    pendingFingerTouch = nil
                    if !cancelled && maxTouchCount == 1 {
                        // Quick single-finger tap that lifted before the 45ms timer
                        pc_finger(h, 0, Float(pendingFingerPos.x), Float(pendingFingerPos.y), pendingFingerMods)
                        pc_finger(h, 2, Float(p.x), Float(p.y), mods)
                    }
                }
                directTouches.remove(t)
                if t === fingerTouch {
                    pc_finger(h, cancelled ? 3 : 2, Float(p.x), Float(p.y), mods)
                    fingerTouch = nil
                }
                if directTouches.isEmpty {
                    // All fingers have lifted: resolve multi-touch tap gestures
                    let tapDuration = CACurrentMediaTime() - touchStartTime
                    if !cancelled && !multiTouchExceededSlop && tapDuration < 0.45 && !pencilNear {
                        if maxTouchCount == 3 {
                            pcLog("gesture: 3-finger tap -> Redo")
                            tapKey(0x1D, mods: 8 | 1) // ⇧⌘Z
                        } else if maxTouchCount == 2 {
                            pcLog("gesture: 2-finger tap -> Undo")
                            tapKey(0x1D, mods: 8)     // ⌘Z
                        }
                    }
                    maxTouchCount = 0
                    touchStartLocations.removeAll()
                    multiTouchExceededSlop = false
                }
            }
        }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) { finish(touches, with: event, cancelled: false) }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) { finish(touches, with: event, cancelled: true) }
}

// MARK: - Apple Pencil Squeeze Floating Curved Arc Palette

final class SqueezePaletteView: UIView {
    struct ToolItem {
        let id: String
        let name: String
        let systemIcon: String
    }

    private static let tools: [ToolItem] = [
        ToolItem(id: "pathSelection", name: "Select", systemIcon: "arrow.up.left"),
        ToolItem(id: "move", name: "Move", systemIcon: "arrow.up.and.down.and.arrow.left.and.right"),
        ToolItem(id: "brush", name: "Brush", systemIcon: "paintbrush.pointed"),
        ToolItem(id: "eraser", name: "Eraser", systemIcon: "eraser"),
        ToolItem(id: "eyedropper", name: "Picker", systemIcon: "eyedropper.halffull"),
        ToolItem(id: "type", name: "Text", systemIcon: "textformat"),
    ]

    private let platter = UIVisualEffectView(effect: UIBlurEffect(style: .systemMaterialDark))
    private let onSelect: (String) -> Void
    private let onDismiss: () -> Void
    private var arcPath: UIBezierPath = UIBezierPath()

    init(centerAt point: CGPoint, in container: UIView, onSelect: @escaping (String) -> Void, onDismiss: @escaping () -> Void) {
        self.onSelect = onSelect
        self.onDismiss = onDismiss
        super.init(frame: container.bounds)
        autoresizingMask = [.flexibleWidth, .flexibleHeight]
        backgroundColor = .clear

        let midRadius: CGFloat = 160.0
        let thickness: CGFloat = 66.0
        let rIn: CGFloat = midRadius - thickness / 2.0
        let rOut: CGFloat = midRadius + thickness / 2.0
        let spanDeg: CGFloat = 120.0
        let spanRad = spanDeg * .pi / 180.0
        let startAngle: CGFloat = -(.pi / 2.0) - (spanRad / 2.0)
        let endAngle: CGFloat = -(.pi / 2.0) + (spanRad / 2.0)

        // Position curvature center so arc curves naturally right above the squeeze point
        var curCenter = CGPoint(x: point.x, y: point.y + 70.0)
        let minX = rOut + 20.0
        let maxX = container.bounds.width - rOut - 20.0
        curCenter.x = max(minX, min(maxX, curCenter.x))
        if curCenter.y - rOut < 40.0 {
            curCenter.y = 40.0 + rOut
        }
        if curCenter.y > container.bounds.height - 30.0 {
            curCenter.y = container.bounds.height - 30.0
        }

        platter.frame = container.bounds
        addSubview(platter)

        let path = SqueezePaletteView.buildArcPath(center: curCenter, rIn: rIn, rOut: rOut, startAngle: startAngle, endAngle: endAngle)
        self.arcPath = path

        let maskLayer = CAShapeLayer()
        maskLayer.path = path.cgPath
        platter.layer.mask = maskLayer

        let borderLayer = CAShapeLayer()
        borderLayer.path = path.cgPath
        borderLayer.fillColor = UIColor.clear.cgColor
        borderLayer.strokeColor = UIColor.white.withAlphaComponent(0.20).cgColor
        borderLayer.lineWidth = 1.0
        layer.addSublayer(borderLayer)

        layer.shadowColor = UIColor.black.cgColor
        layer.shadowOpacity = 0.45
        layer.shadowRadius = 16
        layer.shadowOffset = CGSize(width: 0, height: 6)

        let btnSize: CGFloat = 38.0
        let count = SqueezePaletteView.tools.count
        let marginRad: CGFloat = 0.14
        let usableStart = startAngle + marginRad
        let usableEnd = endAngle - marginRad
        for (i, tool) in SqueezePaletteView.tools.enumerated() {
            let frac = CGFloat(i) / CGFloat(max(1, count - 1))
            let angle = usableStart + (usableEnd - usableStart) * frac
            let bx = curCenter.x + midRadius * cos(angle) - (btnSize / 2.0)
            let by = curCenter.y + midRadius * sin(angle) - (btnSize / 2.0)

            let btn = UIButton(type: .system)
            btn.frame = CGRect(x: bx, y: by, width: btnSize, height: btnSize)
            btn.layer.cornerRadius = btnSize / 2.0
            btn.backgroundColor = UIColor.white.withAlphaComponent(0.12)
            btn.tintColor = .white
            btn.layer.borderColor = UIColor.white.withAlphaComponent(0.18).cgColor
            btn.layer.borderWidth = 0.8

            let img = UIImage(systemName: tool.systemIcon, withConfiguration: UIImage.SymbolConfiguration(pointSize: 16, weight: .medium))
            btn.setImage(img, for: .normal)
            btn.accessibilityLabel = tool.name
            btn.tag = i
            btn.addTarget(self, action: #selector(toolTapped(_:)), for: .touchUpInside)

            addSubview(btn)
        }

        let tapBg = UITapGestureRecognizer(target: self, action: #selector(bgTapped(_:)))
        addGestureRecognizer(tapBg)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    private static func buildArcPath(center: CGPoint, rIn: CGFloat, rOut: CGFloat, startAngle: CGFloat, endAngle: CGFloat) -> UIBezierPath {
        let path = UIBezierPath()
        let capRadius = (rOut - rIn) / 2.0
        let midRadius = (rIn + rOut) / 2.0

        path.addArc(withCenter: center, radius: rIn, startAngle: startAngle, endAngle: endAngle, clockwise: true)

        let endMid = CGPoint(x: center.x + midRadius * cos(endAngle), y: center.y + midRadius * sin(endAngle))
        path.addArc(withCenter: endMid, radius: capRadius, startAngle: endAngle - .pi, endAngle: endAngle, clockwise: false)

        path.addArc(withCenter: center, radius: rOut, startAngle: endAngle, endAngle: startAngle, clockwise: false)

        let startMid = CGPoint(x: center.x + midRadius * cos(startAngle), y: center.y + midRadius * sin(startAngle))
        path.addArc(withCenter: startMid, radius: capRadius, startAngle: startAngle, endAngle: startAngle + .pi, clockwise: false)

        path.close()
        return path
    }

    func animateIn() {
        transform = CGAffineTransform(scaleX: 0.6, y: 0.6)
        alpha = 0.0
        UIView.animate(withDuration: 0.22, delay: 0, usingSpringWithDamping: 0.78, initialSpringVelocity: 0, options: [.curveEaseOut]) {
            self.transform = .identity
            self.alpha = 1.0
        }
    }

    func dismiss() {
        UIView.animate(withDuration: 0.15, animations: {
            self.transform = CGAffineTransform(scaleX: 0.8, y: 0.8)
            self.alpha = 0.0
        }, completion: { _ in
            self.removeFromSuperview()
            self.onDismiss()
        })
    }

    @objc private func toolTapped(_ sender: UIButton) {
        let tool = SqueezePaletteView.tools[sender.tag]
        onSelect(tool.id)
        dismiss()
    }

    @objc private func bgTapped(_ g: UITapGestureRecognizer) {
        let p = g.location(in: self)
        if !arcPath.contains(p) {
            dismiss()
        }
    }
}
