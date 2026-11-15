//
//  LightairCameraStreamSource.swift
//  Lightair Virtual Camera: adapted from OBS Studio's mac-virtualcam camera extension (GPL-2.0)
//
//  Created by Sebastian Beckmann on 2022-09-30.
//  Changed by Patrick Heyer on 2022-10-16.
//

import CoreMediaIO
import Foundation
import os.log

class LightairCameraStreamSource: NSObject, CMIOExtensionStreamSource {
    private(set) var stream: CMIOExtensionStream!

    let device: CMIOExtensionDevice

    private let _streamFormat: CMIOExtensionStreamFormat

    init(
        localizedName: String, streamID: UUID, streamFormat: CMIOExtensionStreamFormat,
        device: CMIOExtensionDevice
    ) {
        self.device = device
        self._streamFormat = streamFormat
        super.init()
        self.stream = CMIOExtensionStream(
            localizedName: localizedName,
            streamID: streamID,
            direction: .source,
            clockType: .hostTime,
            source: self
        )
    }

    var formats: [CMIOExtensionStreamFormat] {
        return [_streamFormat]
    }

    var activeFormatIndex: Int = 0 {
        didSet {
            if activeFormatIndex >= 1 {
                os_log(.error, "Invalid index")
            }
        }
    }

    var availableProperties: Set<CMIOExtensionProperty> {
        return [.streamActiveFormatIndex, .streamFrameDuration]
    }

    func streamProperties(forProperties properties: Set<CMIOExtensionProperty>) throws
        -> CMIOExtensionStreamProperties
    {
        let streamProperties = CMIOExtensionStreamProperties(dictionary: [:])

        if properties.contains(.streamActiveFormatIndex) {
            streamProperties.activeFormatIndex = 0
        }
        if properties.contains(.streamFrameDuration) {
            let frameDuration = CMTime(value: 1, timescale: Int32(LightairCameraFrameRate))
            streamProperties.frameDuration = frameDuration
        }

        return streamProperties
    }

    func setStreamProperties(_ streamProperties: CMIOExtensionStreamProperties) throws {
        if let activeFormatIndex = streamProperties.activeFormatIndex {
            self.activeFormatIndex = activeFormatIndex
        }
    }

    func authorizedToStartStream(for client: CMIOExtensionClient) -> Bool {
        return true
    }

    func startStream() throws {
        guard let deviceSource = device.source as? LightairCameraDeviceSource else {
            fatalError("Unexpected source type \(String(describing: device.source))")
        }

        deviceSource.startStreaming()
    }

    func stopStream() throws {
        guard let deviceSource = device.source as? LightairCameraDeviceSource else {
            fatalError("Unexpcted source type \(String(describing: device.source))")
        }
        deviceSource.stopStreaming()
    }
}
