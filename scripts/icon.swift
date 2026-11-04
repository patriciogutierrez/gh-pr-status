// Draws the app icon: a pull request glyph (a branch merging back) on the
// app's own dark surface, and writes it as a 1024 px PNG.
//
//   swift scripts/icon.swift <out.png>
//
// `scripts/icon.sh` turns that PNG into the .icns the bundle uses.

import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let size: CGFloat = 1024
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon.png"

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    CGColor(
        red: CGFloat((hex >> 16) & 0xff) / 255,
        green: CGFloat((hex >> 8) & 0xff) / 255,
        blue: CGFloat(hex & 0xff) / 255,
        alpha: alpha)
}

let space = CGColorSpace(name: CGColorSpace.sRGB)!
let ctx = CGContext(
    data: nil, width: Int(size), height: Int(size), bitsPerComponent: 8, bytesPerRow: 0,
    space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!

// macOS icons leave a margin inside the canvas; the rounded square fills the
// rest, with Apple's corner ratio.
let inset: CGFloat = size * 0.1
let square = CGRect(x: inset, y: inset, width: size - 2 * inset, height: size - 2 * inset)
let radius = square.width * 0.2237
let shape = CGPath(roundedRect: square, cornerWidth: radius, cornerHeight: radius, transform: nil)

ctx.saveGState()
ctx.addPath(shape)
ctx.clip()
let gradient = CGGradient(
    colorsSpace: space, colors: [color(0x313244), color(0x1e1e2e)] as CFArray, locations: [0, 1])!
ctx.drawLinearGradient(
    gradient, start: CGPoint(x: 0, y: size), end: CGPoint(x: size, y: 0), options: [])
ctx.restoreGState()

// A subtle edge so the square reads on dark backgrounds too.
ctx.addPath(shape)
ctx.setStrokeColor(color(0x585b70, 0.6))
ctx.setLineWidth(6)
ctx.strokePath()

// The glyph: a trunk on the left with a commit at each end, and a branch
// that leaves the top commit and merges into a commit on the right.
let stroke: CGFloat = size * 0.075
let dot: CGFloat = size * 0.075
let left = CGPoint(x: size * 0.36, y: 0)
let top: CGFloat = size * 0.70
let bottom: CGFloat = size * 0.30
let right: CGFloat = size * 0.64

ctx.setLineCap(.round)
ctx.setLineWidth(stroke)

ctx.setStrokeColor(color(0xcba6f7))
ctx.move(to: CGPoint(x: left.x, y: top))
ctx.addLine(to: CGPoint(x: left.x, y: bottom))
ctx.strokePath()

// The branch: out of the top commit, around, and down into the right one.
ctx.setStrokeColor(color(0xa6e3a1))
ctx.move(to: CGPoint(x: left.x + dot * 0.9, y: top))
ctx.addCurve(
    to: CGPoint(x: right, y: top - size * 0.14),
    control1: CGPoint(x: right - size * 0.04, y: top),
    control2: CGPoint(x: right, y: top - size * 0.02))
ctx.addLine(to: CGPoint(x: right, y: bottom + dot * 0.9))
ctx.strokePath()

func commit(_ x: CGFloat, _ y: CGFloat, _ fill: UInt32) {
    let rect = CGRect(x: x - dot, y: y - dot, width: dot * 2, height: dot * 2)
    ctx.setFillColor(color(0x1e1e2e))
    ctx.fillEllipse(in: rect)
    ctx.setStrokeColor(color(fill))
    ctx.setLineWidth(stroke * 0.8)
    ctx.strokeEllipse(in: rect.insetBy(dx: stroke * 0.4, dy: stroke * 0.4))
}

commit(left.x, top, 0xcba6f7)
commit(left.x, bottom, 0xcba6f7)
commit(right, bottom, 0xa6e3a1)

let image = ctx.makeImage()!
let url = URL(fileURLWithPath: out) as CFURL
let dest = CGImageDestinationCreateWithURL(url, UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(dest, image, nil)
CGImageDestinationFinalize(dest)
