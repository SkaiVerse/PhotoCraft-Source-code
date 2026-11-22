import UIKit

/// The on-screen keyboard moves the text cursor when you drag on the space bar (and selects
/// text when a second finger is down), but only for views that describe their text to iPadOS.
/// PhotoCraft's real text lives inside the app, so this describes an imaginary long line of
/// text with the cursor in the middle. Whenever iPadOS moves that imaginary cursor, the same
/// number of Left/Right/Up/Down key presses (with Shift when selecting) go to the app, which
/// moves the real cursor. Typing and the physical keyboard are not affected.
///
/// If typing ever misbehaves, deleting this file (and the four properties near the top of
/// MetalView.swift that mention `vt`, `inputDelegate`, `markedTextStyle`) removes the feature.
final class VirtualText {
    let length = 1_000_000
    var anchor = 500_000
    var focus = 500_000
    var marked: String?
    var floating = false
    var floatLast = CGPoint.zero
    var carryX: CGFloat = 0
    var carryY: CGFloat = 0

    /// Typing moves the imaginary cursor along with the real one.
    func advance(by n: Int) {
        let f = min(max(focus + n, 0), length)
        focus = f
        anchor = f
    }
}

final class VTPosition: UITextPosition {
    let offset: Int
    init(_ offset: Int) {
        self.offset = offset
        super.init()
    }
}

final class VTRange: UITextRange {
    let lo: VTPosition
    let hi: VTPosition
    init(_ a: Int, _ b: Int) {
        lo = VTPosition(min(a, b))
        hi = VTPosition(max(a, b))
        super.init()
    }
    override var start: UITextPosition { lo }
    override var end: UITextPosition { hi }
    override var isEmpty: Bool { lo.offset == hi.offset }
}

final class VTSelectionRect: UITextSelectionRect {
    private let _rect: CGRect
    init(_ rect: CGRect) {
        self._rect = rect
        super.init()
    }
    override var rect: CGRect { _rect }
    override var writingDirection: NSWritingDirection { .natural }
    override var containsStart: Bool { true }
    override var containsEnd: Bool { true }
    override var isVertical: Bool { false }
}

extension MetalView: UITextInput {

    // MARK: Selection (the part the space-bar gesture drives)

    var selectedTextRange: UITextRange? {
        get { wantsKeyboard ? VTRange(vt.anchor, vt.focus) : nil }
        set {
            guard wantsKeyboard, let r = newValue as? VTRange else { return }
            vtSelect(lo: r.lo.offset, hi: r.hi.offset)
            inputDelegate?.selectionDidChange(self)
        }
    }

    /// iPadOS set a new selection: send the matching arrow presses to the app.
    private func vtSelect(lo s: Int, hi e: Int) {
        guard wantsKeyboard else { return }
        if vt.floating && s == e && vt.anchor == vt.focus { return }   // the floating cursor moves the caret by itself
        let oldLo = min(vt.anchor, vt.focus)
        let oldHi = max(vt.anchor, vt.focus)
        if s == oldLo && e == oldHi { return }
        var newAnchor = s
        var newFocus = e
        if s == e {
            newAnchor = s
            newFocus = s
        } else if oldLo == oldHi {
            // A selection growing out of a caret.
            if e != oldHi { newAnchor = s; newFocus = e } else { newAnchor = e; newFocus = s }
        } else if s != oldLo && e == oldHi {
            newFocus = s
            newAnchor = e
        } else if e != oldHi && s == oldLo {
            newFocus = e
            newAnchor = s
        } else if vt.focus >= vt.anchor {
            newFocus = e
            newAnchor = s
        } else {
            newFocus = s
            newAnchor = e
        }
        let dx = newFocus - vt.focus
        let selecting = newAnchor != newFocus
        for _ in 0..<min(abs(dx), 400) {
            tapKey(dx > 0 ? 0x4F : 0x50, mods: selecting ? 1 : 0)   // Right / Left (+ Shift)
        }
        vt.anchor = newAnchor
        vt.focus = newFocus
    }

    // MARK: Floating cursor (dragging on the space bar)

    @objc(beginFloatingCursorAtPoint:)
    func beginFloatingCursor(at point: CGPoint) {
        vt.floating = true
        vt.floatLast = point
        vt.carryX = 0
        vt.carryY = 0
    }

    @objc(updateFloatingCursorAtPoint:)
    func updateFloatingCursor(at point: CGPoint) {
        guard vt.floating, handle != nil else { return }
        vt.carryX += point.x - vt.floatLast.x
        vt.carryY += point.y - vt.floatLast.y
        vt.floatLast = point
        let stepX: CGFloat = 9
        let stepY: CGFloat = 22
        var guardCount = 0
        while vt.carryX >= stepX && guardCount < 60 { tapKey(0x4F, mods: 0); vt.carryX -= stepX; guardCount += 1 }
        while vt.carryX <= -stepX && guardCount < 60 { tapKey(0x50, mods: 0); vt.carryX += stepX; guardCount += 1 }
        while vt.carryY >= stepY && guardCount < 60 { tapKey(0x51, mods: 0); vt.carryY -= stepY; guardCount += 1 }
        while vt.carryY <= -stepY && guardCount < 60 { tapKey(0x52, mods: 0); vt.carryY += stepY; guardCount += 1 }
    }

    @objc(endFloatingCursor)
    func endFloatingCursor() {
        vt.floating = false
    }

    // MARK: The rest of what iPadOS asks about the (imaginary) text

    var markedTextRange: UITextRange? {
        vt.marked == nil ? nil : VTRange(vt.focus, vt.focus)
    }

    var beginningOfDocument: UITextPosition { VTPosition(0) }
    var endOfDocument: UITextPosition { VTPosition(wantsKeyboard ? vt.length : 0) }
    var tokenizer: UITextInputTokenizer { vtTokenizer }
    var textInputView: UIView { self }

    func text(in range: UITextRange) -> String? {
        guard let r = range as? VTRange else { return "" }
        let count = max(0, r.hi.offset - r.lo.offset)
        return count > 0 ? String(repeating: " ", count: count) : ""
    }

    func replace(_ range: UITextRange, withText text: String) {
        insertText(text)
    }

    /// Text being composed (for example with a Chinese or Japanese keyboard) is held until it is
    /// committed, then typed into the app.
    func setMarkedText(_ markedText: String?, selectedRange: NSRange) {
        vt.marked = markedText
    }

    func unmarkText() {
        if let text = vt.marked, !text.isEmpty {
            vt.marked = nil
            insertText(text)
        }
        vt.marked = nil
    }

    func textRange(from fromPosition: UITextPosition, to toPosition: UITextPosition) -> UITextRange? {
        guard let a = fromPosition as? VTPosition, let b = toPosition as? VTPosition else { return nil }
        return VTRange(a.offset, b.offset)
    }

    func position(from position: UITextPosition, offset: Int) -> UITextPosition? {
        guard let p = position as? VTPosition else { return nil }
        return VTPosition(min(max(p.offset + offset, 0), vt.length))
    }

    func position(from position: UITextPosition, in direction: UITextLayoutDirection, offset: Int) -> UITextPosition? {
        guard let p = position as? VTPosition else { return nil }
        var delta = offset
        switch direction {
        case .right: delta = offset
        case .left: delta = -offset
        case .up: delta = -offset * 40
        case .down: delta = offset * 40
        @unknown default: delta = offset
        }
        return VTPosition(min(max(p.offset + delta, 0), vt.length))
    }

    func compare(_ position: UITextPosition, to other: UITextPosition) -> ComparisonResult {
        guard let a = position as? VTPosition, let b = other as? VTPosition else { return .orderedSame }
        if a.offset < b.offset { return .orderedAscending }
        if a.offset > b.offset { return .orderedDescending }
        return .orderedSame
    }

    func offset(from: UITextPosition, to toPosition: UITextPosition) -> Int {
        guard let a = from as? VTPosition, let b = toPosition as? VTPosition else { return 0 }
        return b.offset - a.offset
    }

    func position(within range: UITextRange, farthestIn direction: UITextLayoutDirection) -> UITextPosition? {
        guard let r = range as? VTRange else { return nil }
        switch direction {
        case .right, .down: return r.hi
        case .left, .up: return r.lo
        @unknown default: return r.hi
        }
    }

    func characterRange(byExtending position: UITextPosition, in direction: UITextLayoutDirection) -> UITextRange? {
        guard let p = position as? VTPosition else { return nil }
        switch direction {
        case .right, .down: return VTRange(p.offset, min(p.offset + 1, vt.length))
        case .left, .up: return VTRange(max(p.offset - 1, 0), p.offset)
        @unknown default: return VTRange(p.offset, p.offset)
        }
    }

    func baseWritingDirection(for position: UITextPosition, in direction: UITextStorageDirection) -> NSWritingDirection {
        .natural
    }

    func setBaseWritingDirection(_ writingDirection: NSWritingDirection, for range: UITextRange) {}

    func firstRect(for range: UITextRange) -> CGRect {
        CGRect(x: bounds.midX, y: bounds.midY, width: 2, height: 24)
    }

    func caretRect(for position: UITextPosition) -> CGRect {
        CGRect(x: bounds.midX, y: bounds.midY, width: 2, height: 24)
    }

    func selectionRects(for range: UITextRange) -> [UITextSelectionRect] {
        [VTSelectionRect(firstRect(for: range))]
    }

    func closestPosition(to point: CGPoint) -> UITextPosition? { VTPosition(vt.focus) }

    func closestPosition(to point: CGPoint, within range: UITextRange) -> UITextPosition? { VTPosition(vt.focus) }

    func characterRange(at point: CGPoint) -> UITextRange? { VTRange(vt.focus, vt.focus) }
}
