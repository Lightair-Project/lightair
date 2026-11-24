// lightair-speech: what the microphone hears, as text, for voice-phrase automations
// (crates/lightair-app/src/triggers.rs). Apple's speech recognition, on this Mac when the
// language allows (never sent anywhere then). One line per update on stdout:
//   TEXT <what's been said so far in this utterance>
//   DONE            (the utterance ended; the next TEXT starts a new one)
//   READY <language> <on-device|online>   (once, when it's listening)
//   ERROR <why>
// Runs until its stdin closes (Lightair quitting or no automation listening any more).
// Arguments, both optional: a language ("en-US"; "-" for the system's) and where the sound comes
// from: a Core Audio input device's UID, "-pcm" for 16 kHz mono 16-bit samples on stdin (a mixer
// track Lightair taps), or nothing for the default input.

import AVFoundation
import AudioToolbox
import CoreAudio
import Foundation
import Speech

setvbuf(stdout, nil, _IOLBF, 0)

let args = CommandLine.arguments
let locale = args.count > 1 && args[1] != "-" && !args[1].isEmpty ? Locale(identifier: args[1]) : Locale.current
let fromStdin = args.count > 2 && args[2] == "-pcm"
let deviceUID = args.count > 2 && !args[2].isEmpty && args[2] != "default" && !fromStdin ? args[2] : nil
let pcmFormat = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: 16000, channels: 1, interleaved: false)!
var pending = Data()

/// A Core Audio device by its UID.
func device(uid: String) -> AudioDeviceID? {
    var address = AudioObjectPropertyAddress(
        mSelector: kAudioHardwarePropertyTranslateUIDToDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain)
    var cf = uid as CFString
    var id = AudioDeviceID(0)
    var size = UInt32(MemoryLayout<AudioDeviceID>.size)
    let status = withUnsafePointer(to: &cf) { p in
        AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address, UInt32(MemoryLayout<CFString>.size), p, &size, &id)
    }
    return status == noErr && id != 0 ? id : nil
}
var recognizer: SFSpeechRecognizer?
let engine = AVAudioEngine()
var request: SFSpeechAudioBufferRecognitionRequest?
var task: SFSpeechRecognitionTask?
var started = Date()
var lastError = ""
// the utterance so far and when it last changed: Apple's recognizer rarely ends an utterance in a
// stream that never stops, so it's ended here once the words stop changing
var lastText = ""
var lastTextAt = Date()

func say(_ line: String) {
    print(line.replacingOccurrences(of: "\n", with: " "))
}

/// Which recognition task is the current one: an ended task's last callbacks are ignored.
var generation = 0

func restart() {
    generation += 1
    let mine = generation
    task?.cancel()
    task = nil
    request?.endAudio()
    let req = SFSpeechAudioBufferRecognitionRequest()
    req.shouldReportPartialResults = true
    if recognizer?.supportsOnDeviceRecognition == true {
        req.requiresOnDeviceRecognition = true
    }
    request = req
    started = Date()
    task = recognizer?.recognitionTask(with: req) { result, error in
        guard mine == generation else { return }
        if let r = result {
            lastError = ""
            let text = r.bestTranscription.formattedString
            if text != lastText {
                lastText = text
                lastTextAt = Date()
                say("TEXT " + text)
            }
            if r.isFinal {
                if !lastText.isEmpty { say("DONE") }
                lastText = ""
                DispatchQueue.main.async { restart() }
            }
        } else if let e = error as NSError? {
            // say why it can't hear (once per reason), but not the everyday ones: a task we ended
            // ourselves, a stretch with no speech
            let quiet: Set<Int> = [203, 216, 301, 1101, 1110]
            if !quiet.contains(e.code) {
                let why = "ERROR \(e.localizedDescription) (\(e.domain) \(e.code))"
                if why != lastError {
                    lastError = why
                    say(why)
                }
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { restart() }
        }
    }
}

/// Ends the utterance when its words haven't changed for 1.2 s, and starts listening afresh.
func endWhenSettled() {
    Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { _ in
        if !lastText.isEmpty && Date().timeIntervalSince(lastTextAt) > 1.2 {
            lastText = ""
            say("DONE")
            restart()
        }
    }
}

func start() {
    guard let r = SFSpeechRecognizer(locale: locale), r.isAvailable else {
        say("ERROR speech recognition isn't available for \(locale.identifier)")
        exit(3)
    }
    recognizer = r
    say("READY \(locale.identifier) \(r.supportsOnDeviceRecognition ? "on-device" : "online")")
    // samples from Lightair on stdin: no microphone of our own
    if fromStdin {
        restart()
        endWhenSettled()
        Timer.scheduledTimer(withTimeInterval: 5, repeats: true) { _ in
            if Date().timeIntervalSince(started) > 50 {
                if !lastText.isEmpty { say("DONE") }
                lastText = ""
                restart()
            }
        }
        return
    }
    let input = engine.inputNode
    // a chosen microphone instead of the default input
    if let uid = deviceUID, var id = device(uid: uid), let unit = input.audioUnit {
        AudioUnitSetProperty(unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, &id, UInt32(MemoryLayout<AudioDeviceID>.size))
    }
    let format = input.outputFormat(forBus: 0)
    input.removeTap(onBus: 0)
    input.installTap(onBus: 0, bufferSize: 1024, format: format) { buffer, _ in
        request?.append(buffer)
    }
    engine.prepare()
    do {
        try engine.start()
    } catch {
        say("ERROR the microphone didn't start: \(error.localizedDescription)")
        exit(4)
    }
    restart()
    endWhenSettled()
    // a recognition task lasts about a minute: start a new one before then
    Timer.scheduledTimer(withTimeInterval: 5, repeats: true) { _ in
        if Date().timeIntervalSince(started) > 50 {
            if !lastText.isEmpty { say("DONE") }
            lastText = ""
            restart()
        }
    }
}

// stdin: samples (with -pcm) or nothing; closing means Lightair doesn't want us any more
FileHandle.standardInput.readabilityHandler = { h in
    let data = h.availableData
    if data.isEmpty { exit(0) }
    guard fromStdin else { return }
    pending.append(data)
    let count = pending.count / 2
    guard count > 0, let buffer = AVAudioPCMBuffer(pcmFormat: pcmFormat, frameCapacity: AVAudioFrameCount(count)) else { return }
    buffer.frameLength = AVAudioFrameCount(count)
    let out = buffer.floatChannelData![0]
    pending.withUnsafeBytes { raw in
        let samples = raw.bindMemory(to: Int16.self)
        for i in 0..<count { out[i] = Float(Int16(littleEndian: samples[i])) / 32768.0 }
    }
    pending.removeFirst(count * 2)
    DispatchQueue.main.async { request?.append(buffer) }
}

SFSpeechRecognizer.requestAuthorization { status in
    guard status == .authorized else {
        say("ERROR Lightair may not use speech recognition (System Settings › Privacy & Security › Speech Recognition)")
        exit(2)
    }
    DispatchQueue.main.async { start() }
}

RunLoop.main.run()
