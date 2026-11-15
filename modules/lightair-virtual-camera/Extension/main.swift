//
//  main.swift
//  Lightair Virtual Camera: adapted from OBS Studio's mac-virtualcam camera extension (GPL-2.0)
//
//  Created by Sebastian Beckmann on 2022-09-30.
//  Changed by Patrick Heyer on 2022-10-16.
//

import CoreMediaIO
import Foundation
import os.log

let LightairCameraDeviceUUID = Bundle.main.object(forInfoDictionaryKey: "LightairCameraDeviceUUID") as? String
let LightairCameraSourceUUID = Bundle.main.object(forInfoDictionaryKey: "LightairCameraSourceUUID") as? String
let LightairCameraSinkUUID = Bundle.main.object(forInfoDictionaryKey: "LightairCameraSinkUUID") as? String

guard let LightairCameraDeviceUUID = LightairCameraDeviceUUID, let LightairCameraSourceUUID = LightairCameraSourceUUID,
    let LightairCameraSinkUUID = LightairCameraSinkUUID
else {
    fatalError("Unable to retrieve Camera Extension UUIDs from Info.plist.")
}

guard let deviceUUID = UUID(uuidString: LightairCameraDeviceUUID), let sourceUUID = UUID(uuidString: LightairCameraSourceUUID),
    let sinkUUID = UUID(uuidString: LightairCameraSinkUUID)
else {
    fatalError("Unable to generate Camera Extension UUIDs from Info.plist values.")
}

let providerSource = LightairCameraProviderSource(
    clientQueue: nil, deviceUUID: deviceUUID, sourceUUID: sourceUUID, sinkUUID: sinkUUID)
CMIOExtensionProvider.startService(provider: providerSource.provider)
CFRunLoopRun()
