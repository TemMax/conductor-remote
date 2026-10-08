// Draws the placeholder app icon and writes the ten macOS sizes with their Contents.json.
// Usage: swift scripts/make-placeholder-icon.swift <appiconset dir>
import AppKit

guard CommandLine.arguments.count == 2 else {
    FileHandle.standardError.write(Data("usage: make-placeholder-icon.swift <appiconset dir>\n".utf8))
    exit(2)
}
let directory = URL(filePath: CommandLine.arguments[1], directoryHint: .isDirectory)

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("error: \(message)\n".utf8))
    exit(1)
}

func color(_ hex: UInt32) -> NSColor {
    NSColor(srgbRed: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255,
            blue: CGFloat(hex & 0xFF) / 255, alpha: 1)
}

// The macOS icon grid, on a 1024 px canvas.
let canvas: CGFloat = 1024
let margin: CGFloat = 100
let body: CGFloat = 824
let cornerRadius: CGFloat = 185
let symbolShare: CGFloat = 0.55
let symbolName = "iphone.radiowaves.left.and.right"

let configuration = NSImage.SymbolConfiguration(pointSize: 400, weight: .medium)
    .applying(NSImage.SymbolConfiguration(paletteColors: [.white]))
guard let symbol = NSImage(systemSymbolName: symbolName, accessibilityDescription: nil)?
    .withSymbolConfiguration(configuration) else {
    fail("the SF Symbol \(symbolName) is not available")
}

func png(pixels: Int) -> Data {
    guard let bitmap = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels, bitsPerSample: 8,
        samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
        bytesPerRow: 0, bitsPerPixel: 0),
        let context = NSGraphicsContext(bitmapImageRep: bitmap) else {
        fail("could not make a \(pixels) px bitmap")
    }
    bitmap.size = NSSize(width: pixels, height: pixels)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = context
    context.imageInterpolation = .high
    let transform = NSAffineTransform()
    transform.scale(by: CGFloat(pixels) / canvas)
    transform.concat()

    let bodyRect = NSRect(x: margin, y: margin, width: body, height: body)
    let shape = NSBezierPath(roundedRect: bodyRect, xRadius: cornerRadius, yRadius: cornerRadius)
    // 90°: the first colour at the bottom, the second at the top.
    NSGradient(starting: color(0x1E2A78), ending: color(0x3B5BDB))?.draw(in: shape, angle: 90)

    let side = body * symbolShare
    let scale = side / max(symbol.size.width, symbol.size.height)
    let size = NSSize(width: symbol.size.width * scale, height: symbol.size.height * scale)
    let symbolRect = NSRect(x: (canvas - size.width) / 2, y: (canvas - size.height) / 2,
                            width: size.width, height: size.height)
    symbol.draw(in: symbolRect, from: .zero, operation: .sourceOver, fraction: 1)

    NSGraphicsContext.restoreGraphicsState()
    guard let data = bitmap.representation(using: .png, properties: [:]) else {
        fail("could not encode the \(pixels) px icon")
    }
    return data
}

do {
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    var images: [[String: String]] = []
    for points in [16, 32, 128, 256, 512] {
        for scale in [1, 2] {
            let name = scale == 1 ? "icon_\(points)x\(points).png" : "icon_\(points)x\(points)@2x.png"
            try png(pixels: points * scale).write(to: directory.appending(path: name))
            images.append(["filename": name, "idiom": "mac", "scale": "\(scale)x", "size": "\(points)x\(points)"])
        }
    }
    let contents: [String: Any] = ["images": images, "info": ["author": "xcode", "version": 1]]
    var json = try JSONSerialization.data(withJSONObject: contents, options: [.prettyPrinted, .sortedKeys])
    json.append(Data("\n".utf8))
    try json.write(to: directory.appending(path: "Contents.json"))
} catch {
    fail("\(error.localizedDescription)")
}
