import Foundation
import VideoToolbox
import CoreMedia
import CoreVideo

/// Decodes the H.264 (Annex-B / Baseline) stream coming from the PC server.
///
/// The PC prepends SPS/PPS on every keyframe. Each `MSG_VIDEO` payload therefore
/// carries one complete Annex-B access unit. We split it into NAL units, rebuild
/// the parameter set (and decompression session) whenever SPS/PPS change, and feed
/// VCL units (IDR / P slices) to a `VTDecompressionSession`. Decoded frames are
/// delivered on a background queue via `onDecodedFrame`.
final class H264Decoder {

    /// Called on a background queue for each decoded frame.
    var onDecodedFrame: ((CVPixelBuffer) -> Void)?

    private var formatDescription: CMVideoFormatDescription?
    private var session: VTDecompressionSession?
    private var sps: Data?
    private var pps: Data?

    private let queue = DispatchQueue(label: "com.touchmonitor.h264decoder")

    deinit {
        if let session = session {
            VTDecompressionSessionInvalidate(session)
        }
    }

    /// Feed one Annex-B access unit (from a `MSG_VIDEO` payload).
    func decode(accessUnit: Data) {
        queue.async { [self] in
            let nalus = H264NAL.splitAnnexB(accessUnit)
            var didChangeParams = false
            var vclUnits: [Data] = []
            for nalu in nalus {
                guard let header = nalu.first else { continue }
                let type = header & 0x1F
                switch type {
                case 7: // SPS
                    if sps != nalu {
                        sps = nalu
                        didChangeParams = true
                    }
                case 8: // PPS
                    if pps != nalu {
                        pps = nalu
                        didChangeParams = true
                    }
                case 5, 1: // IDR / non-IDR slice
                    vclUnits.append(nalu)
                default:
                    break // SEI, AUD, etc. — skip
                }
            }
            if vclUnits.isEmpty { return }

            if didChangeParams || session == nil {
                guard let sps = sps, let pps = pps else { return }
                rebuildSession(sps: sps, pps: pps)
            }
            guard let session = session,
                  let formatDescription = formatDescription else { return }

            guard let blockBuffer = makeBlockBuffer(nalUnits: vclUnits) else { return }

            var timingInfo = CMSampleTimingInfo()
            var sampleBuffer: CMSampleBuffer?
            let status = CMSampleBufferCreateReady(
                allocator: kCFAllocatorDefault,
                dataBuffer: blockBuffer,
                formatDescription: formatDescription,
                sampleCount: 1,
                sampleTimingEntryCount: 1,
                sampleTimingArray: &timingInfo,
                sampleSizeEntryCount: 0,
                sampleSizeArray: nil,
                sampleBufferOut: &sampleBuffer
            )
            guard status == noErr, let sampleBuffer = sampleBuffer else { return }

            var infoFlags = VTDecodeInfoFlags()
            VTDecompressionSessionDecodeFrame(
                session,
                sampleBuffer: sampleBuffer,
                flags: VTDecodeFrameFlags(rawValue: 0),
                frameRefcon: nil,
                infoFlagsOut: &infoFlags
            )
        }
    }

    private func rebuildSession(sps: Data, pps: Data) {
        var descOut: CMVideoFormatDescription?
        let status = sps.withUnsafeBytes { spsBytes in
            pps.withUnsafeBytes { ppsBytes in
                guard let spsBase = spsBytes.baseAddress, let ppsBase = ppsBytes.baseAddress else {
                    return -12711 // kCMFormatDescriptionBridgeError_InvalidParameter
                }
                var pointers: [UnsafePointer<UInt8>] = [
                    spsBase.assumingMemoryBound(to: UInt8.self),
                    ppsBase.assumingMemoryBound(to: UInt8.self),
                ]
                var sizes: [Int] = [spsBytes.count, ppsBytes.count]
                return Int(CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: kCFAllocatorDefault,
                    parameterSetCount: 2,
                    parameterSetPointers: &pointers,
                    parameterSetSizes: &sizes,
                    nalUnitHeaderLength: 4,
                    formatDescriptionOut: &descOut
                ))
            }
        }
        guard status == noErr, let desc = descOut else { return }
        formatDescription = desc

        if let old = session {
            VTDecompressionSessionInvalidate(old)
            session = nil
        }

        var callback = VTDecompressionOutputCallbackRecord(
            decompressionOutputCallback: decoderOutputCallback,
            decompressionOutputRefCon: Unmanaged.passUnretained(self).toOpaque()
        )
        var newSession: VTDecompressionSession?
        let sessionStatus = VTDecompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            formatDescription: desc,
            decoderSpecification: nil,
            imageBufferAttributes: nil,
            outputCallback: &callback,
            decompressionSessionOut: &newSession
        )
        guard sessionStatus == noErr, let newSession = newSession else { return }

        // Deliver frames quickly for interactive use.
        VTSessionSetProperty(newSession, key: kVTDecompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        session = newSession
    }

    private func makeBlockBuffer(nalUnits: [Data]) -> CMBlockBuffer? {
        // Concatenate NALs as AVCC (4-byte big-endian length), matching the
        // "nalUnitHeaderLength = 4" used when creating the format description.
        var data = Data()
        for nalu in nalUnits {
            var len = UInt32(nalu.count).bigEndian
            withUnsafeBytes(of: &len) { data.append(contentsOf: $0) }
            data.append(nalu)
        }
        guard !data.isEmpty else { return nil }

        // Copy the bytes into a CFData so the block buffer can own its backing
        // memory; the block buffer frees the CFData when it is destroyed.
        let cfData = data.withUnsafeBytes { raw -> CFData? in
            guard let base = raw.baseAddress else { return nil }
            return CFDataCreate(kCFAllocatorDefault, base.assumingMemoryBound(to: UInt8.self), data.count)
        }
        guard let cfData = cfData else { return nil }

        var customSource = CMBlockBufferCustomBlockSource(
            version: 0,
            AllocateBlock: nil,
            FreeBlock: { refCon, _, _ in
                if let refCon = refCon {
                    Unmanaged<CFData>.fromOpaque(refCon).release()
                }
            },
            refCon: Unmanaged.passRetained(cfData).toOpaque()
        )

        var blockBuffer: CMBlockBuffer?
        let bytePtr = CFDataGetBytePtr(cfData)
        let memoryBlock = bytePtr.map { UnsafeMutableRawPointer(mutating: $0) }
        let status = CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault,
            memoryBlock: memoryBlock,
            blockLength: CFDataGetLength(cfData),
            blockAllocator: kCFAllocatorNull,
            customBlockSource: &customSource,
            offsetToData: 0,
            dataLength: CFDataGetLength(cfData),
            flags: 0,
            blockBufferOut: &blockBuffer
        )
        return status == kCMBlockBufferNoErr ? blockBuffer : nil
    }
}

/// Splits an Annex-B H.264 byte stream into NAL units on start-code boundaries.
enum H264NAL {
    static func splitAnnexB(_ data: Data) -> [Data] {
        var units: [Data] = []
        let bytes = [UInt8](data)
        let count = bytes.count
        var start: Int? = nil
        var i = 0

        func startCode(at idx: Int) -> (length: Int, after: Int)? {
            if idx + 3 < count && bytes[idx] == 0 && bytes[idx+1] == 0 && bytes[idx+2] == 0 && bytes[idx+3] == 1 {
                return (4, idx + 4)
            }
            if idx + 2 < count && bytes[idx] == 0 && bytes[idx+1] == 0 && bytes[idx+2] == 1 {
                return (3, idx + 3)
            }
            return nil
        }

        while i < count {
            if let code = startCode(at: i) {
                if let s = start {
                    let lo = min(s, count)
                    let hi = min(i, count)
                    if lo <= hi {
                        units.append(Data(bytes[lo..<hi]))
                    }
                }
                start = code.after
                i = code.after
            } else {
                i += 1
            }
        }
        if let s = start, s < count {
            units.append(Data(bytes[s..<count]))
        }
        return units
    }
}

/// The actual VideoToolbox output callback. The `sourceFrameRefCon` we set is the
/// decoder instance itself (via `Unmanaged`), which lets us reach its `onDecodedFrame`.
private func decoderOutputCallback(
    _ decompressionOutputRefCon: UnsafeMutableRawPointer?,
    _ sourceFrameRefCon: UnsafeMutableRawPointer?,
    _ status: OSStatus,
    _ infoFlags: VTDecodeInfoFlags,
    _ imageBuffer: CVImageBuffer?,
    _ presentationTimeStamp: CMTime,
    _ presentationDuration: CMTime
) {
    guard let decoderRef = decompressionOutputRefCon,
          let imageBuffer = imageBuffer else { return }
    let decoder = Unmanaged<H264Decoder>.fromOpaque(decoderRef).takeUnretainedValue()
    let pixelBuffer = imageBuffer as CVPixelBuffer
    decoder.onDecodedFrame?(pixelBuffer)
}
