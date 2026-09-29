// One AU through a fresh VideoDecoder, waiting on its async output callback: the harness every
// decode-path test shares.

import CoreMedia
import XCTest
@testable import PunktfunkKit

/// Sendable holder for what the decoder's background callback writes.
private final class FrameBox: @unchecked Sendable {
    let lock = NSLock()
    var frame: ReadyFrame?
    var error: OSStatus?
}

/// Decode `au` and return the delivered frame. `configure` sets codec, chroma or depth before
/// the submit. Fails the test on a refused submit, a decode error or a 10 s silence.
func decodeOnce(
    _ au: AccessUnit, format: CMVideoFormatDescription,
    configure: (VideoDecoder) -> Void = { _ in }
) throws -> ReadyFrame {
    let box = FrameBox()
    let done = DispatchSemaphore(value: 0)
    let decoder = VideoDecoder(
        onDecoded: { f in box.lock.lock(); box.frame = f; box.lock.unlock(); done.signal() },
        onDecodeError: { s in box.lock.lock(); box.error = s; box.lock.unlock(); done.signal() })
    configure(decoder)
    XCTAssertTrue(decoder.decode(au: au, format: format), "frame submit should succeed")
    XCTAssertEqual(done.wait(timeout: .now() + 10), .success, "the decode callback must fire")
    decoder.reset()

    box.lock.lock(); let frame = box.frame; let error = box.error; box.lock.unlock()
    XCTAssertNil(error.map { "decode error \($0)" })
    return try XCTUnwrap(frame, "the decode callback must deliver a ReadyFrame")
}
