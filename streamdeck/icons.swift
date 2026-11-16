// Draws the Stream Deck plugin's images (run: swift streamdeck/icons.swift), in Lightair's
// colours (docs/06-design-system.md): keys on bg.panel, on air in state.live, recording in
// state.recording, muted in state.warn. Action-list icons are white on transparent.

import AppKit
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers

let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("app.lightair.sdPlugin/imgs")

func rgb(_ hex: UInt32) -> CGColor {
    CGColor(red: CGFloat((hex >> 16) & 255) / 255, green: CGFloat((hex >> 8) & 255) / 255, blue: CGFloat(hex & 255) / 255, alpha: 1)
}

let panel = rgb(0x1B1B1C), light = rgb(0xEDEDED), ink = rgb(0x141414)
let live = rgb(0xFF3B4A), recording = rgb(0xFF5A3D), warn = rgb(0xFBBF24)

/// Draws a glyph in a unit square (y down) with `fg`.
typealias Glyph = (CGContext, CGColor) -> Void

func save(_ name: String, size: Int, bg: CGColor?, fg: CGColor, inset: CGFloat, glyph: Glyph) {
    let ctx = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0, space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    if let bg {
        ctx.setFillColor(bg)
        ctx.fill(CGRect(x: 0, y: 0, width: size, height: size))
    }
    let s = CGFloat(size)
    // unit square, y down, inset
    ctx.translateBy(x: s * inset, y: s * (1 - inset))
    ctx.scaleBy(x: s * (1 - 2 * inset), y: -s * (1 - 2 * inset))
    ctx.setLineCap(.round)
    ctx.setLineJoin(.round)
    glyph(ctx, fg)
    let url = root.appendingPathComponent(name)
    try? FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
    let dest = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(dest, ctx.makeImage()!, nil)
    CGImageDestinationFinalize(dest)
}

let scene: Glyph = { c, fg in
    c.setStrokeColor(fg)
    c.setLineWidth(0.08)
    c.addPath(CGPath(roundedRect: CGRect(x: 0.04, y: 0.12, width: 0.62, height: 0.46), cornerWidth: 0.07, cornerHeight: 0.07, transform: nil))
    c.strokePath()
    c.setFillColor(fg)
    c.addPath(CGPath(roundedRect: CGRect(x: 0.32, y: 0.40, width: 0.64, height: 0.48), cornerWidth: 0.07, cornerHeight: 0.07, transform: nil))
    c.fillPath()
}

let stream: Glyph = { c, fg in
    c.setFillColor(fg)
    c.fillEllipse(in: CGRect(x: 0.4, y: 0.4, width: 0.2, height: 0.2))
    c.setStrokeColor(fg)
    c.setLineWidth(0.08)
    for r in [0.24, 0.42] as [CGFloat] {
        for (start, end) in [(-0.85, 0.85), (Double.pi - 0.85, Double.pi + 0.85)] {
            c.addArc(center: CGPoint(x: 0.5, y: 0.5), radius: r, startAngle: CGFloat(start), endAngle: CGFloat(end), clockwise: false)
            c.strokePath()
        }
    }
}

let record: Glyph = { c, fg in
    c.setFillColor(fg)
    c.fillEllipse(in: CGRect(x: 0.18, y: 0.18, width: 0.64, height: 0.64))
}

let clip: Glyph = { c, fg in
    // a circular arrow around a play triangle (y is down here, so growing angles go clockwise)
    c.setStrokeColor(fg)
    c.setLineWidth(0.08)
    let r: CGFloat = 0.4, start = -0.75 * CGFloat.pi, end = 0.8 * CGFloat.pi
    c.addArc(center: CGPoint(x: 0.5, y: 0.5), radius: r, startAngle: start, endAngle: end, clockwise: false)
    c.strokePath()
    let p = CGPoint(x: 0.5 + r * cos(end), y: 0.5 + r * sin(end))
    let along = CGPoint(x: -sin(end), y: cos(end)), out = CGPoint(x: cos(end), y: sin(end))
    c.setFillColor(fg)
    c.move(to: CGPoint(x: p.x + along.x * 0.16, y: p.y + along.y * 0.16))
    c.addLine(to: CGPoint(x: p.x + out.x * 0.12, y: p.y + out.y * 0.12))
    c.addLine(to: CGPoint(x: p.x - out.x * 0.12, y: p.y - out.y * 0.12))
    c.closePath()
    c.fillPath()
    c.move(to: CGPoint(x: 0.42, y: 0.34))
    c.addLine(to: CGPoint(x: 0.68, y: 0.5))
    c.addLine(to: CGPoint(x: 0.42, y: 0.66))
    c.closePath()
    c.fillPath()
}

let automation: Glyph = { c, fg in
    c.setFillColor(fg)
    let pts: [(CGFloat, CGFloat)] = [(0.58, 0.02), (0.16, 0.56), (0.46, 0.56), (0.38, 0.98), (0.84, 0.40), (0.53, 0.40)]
    c.move(to: CGPoint(x: pts[0].0, y: pts[0].1))
    for p in pts.dropFirst() { c.addLine(to: CGPoint(x: p.0, y: p.1)) }
    c.closePath()
    c.fillPath()
}

func speaker(_ c: CGContext, _ fg: CGColor) {
    c.setFillColor(fg)
    let pts: [(CGFloat, CGFloat)] = [(0.04, 0.36), (0.22, 0.36), (0.46, 0.14), (0.46, 0.86), (0.22, 0.64), (0.04, 0.64)]
    c.move(to: CGPoint(x: pts[0].0, y: pts[0].1))
    for p in pts.dropFirst() { c.addLine(to: CGPoint(x: p.0, y: p.1)) }
    c.closePath()
    c.fillPath()
    c.setStrokeColor(fg)
    c.setLineWidth(0.08)
}

let mute: Glyph = { c, fg in
    speaker(c, fg)
    for r in [0.16, 0.32] as [CGFloat] {
        c.addArc(center: CGPoint(x: 0.5, y: 0.5), radius: r, startAngle: -0.8, endAngle: 0.8, clockwise: false)
        c.strokePath()
    }
}

let muted: Glyph = { c, fg in
    speaker(c, fg)
    c.move(to: CGPoint(x: 0.62, y: 0.34))
    c.addLine(to: CGPoint(x: 0.94, y: 0.66))
    c.move(to: CGPoint(x: 0.94, y: 0.34))
    c.addLine(to: CGPoint(x: 0.62, y: 0.66))
    c.strokePath()
}

let white = rgb(0xFFFFFF)
let glyphs: [(String, Glyph)] = [("scene", scene), ("stream", stream), ("record", record), ("clip", clip), ("automation", automation), ("mute", mute)]
for (name, glyph) in glyphs {
    save("actions/\(name).png", size: 20, bg: nil, fg: white, inset: 0.06, glyph: glyph)
    save("actions/\(name)@2x.png", size: 40, bg: nil, fg: white, inset: 0.06, glyph: glyph)
    let fg = name == "record" ? recording : light
    save("keys/\(name).png", size: 72, bg: panel, fg: fg, inset: 0.28, glyph: glyph)
    save("keys/\(name)@2x.png", size: 144, bg: panel, fg: fg, inset: 0.28, glyph: glyph)
}
let on: [(String, CGColor, CGColor, Glyph)] = [("scene-on", live, white, scene), ("stream-on", live, white, stream), ("record-on", recording, white, record), ("mute-on", warn, ink, muted)]
for (name, bg, fg, glyph) in on {
    save("keys/\(name).png", size: 72, bg: bg, fg: fg, inset: 0.28, glyph: glyph)
    save("keys/\(name)@2x.png", size: 144, bg: bg, fg: fg, inset: 0.28, glyph: glyph)
}
// the category in Stream Deck's list: Lightair's mark would be lost at 28 px, so a broadcast glyph
save("category.png", size: 28, bg: nil, fg: white, inset: 0.08, glyph: stream)
save("category@2x.png", size: 56, bg: nil, fg: white, inset: 0.08, glyph: stream)
print("drew into \(root.path)")
