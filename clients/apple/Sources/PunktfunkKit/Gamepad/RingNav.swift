/// What a pad does to the in-stream quick-action ring while the ring owns the pad
/// (design/touch-client-overlay.md §2.6): the D-pad or left stick step the highlight, A fires
/// it, B backs out, Y returns the highlight to the centre.
public enum RingNav: Sendable {
    case up, down, left, right, confirm, back, centre
}
