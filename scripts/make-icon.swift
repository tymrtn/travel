import AppKit

// Local vector artwork: a folded itinerary with a warm departure arrow.
let output = CommandLine.arguments[1]
try FileManager.default.createDirectory(atPath: output, withIntermediateDirectories: true)
for size in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let pixels = size * scale
        let image = NSImage(size: NSSize(width: pixels, height: pixels))
        image.lockFocus()
        let context = NSGraphicsContext.current!.cgContext
        context.scaleBy(x: CGFloat(pixels) / 1024, y: CGFloat(pixels) / 1024)
        let tile = NSBezierPath(roundedRect: NSRect(x: 42, y: 42, width: 940, height: 940), xRadius: 210, yRadius: 210)
        NSGradient(starting: NSColor(calibratedRed: 0.08, green: 0.29, blue: 0.31, alpha: 1), ending: NSColor(calibratedRed: 0.025, green: 0.12, blue: 0.17, alpha: 1))!.draw(in: tile, angle: -65)
        let paper = NSBezierPath()
        paper.move(to: NSPoint(x: 215, y: 265)); paper.line(to: NSPoint(x: 405, y: 315))
        paper.line(to: NSPoint(x: 605, y: 265)); paper.line(to: NSPoint(x: 800, y: 330))
        paper.line(to: NSPoint(x: 800, y: 745)); paper.line(to: NSPoint(x: 605, y: 680))
        paper.line(to: NSPoint(x: 405, y: 730)); paper.line(to: NSPoint(x: 215, y: 680)); paper.close()
        NSColor(calibratedRed: 0.96, green: 0.94, blue: 0.86, alpha: 1).setFill(); paper.fill()
        NSColor(calibratedRed: 0.70, green: 0.79, blue: 0.74, alpha: 1).setStroke()
        let folds = NSBezierPath(); folds.lineWidth = 8
        folds.move(to: NSPoint(x: 405, y: 315)); folds.line(to: NSPoint(x: 405, y: 730))
        folds.move(to: NSPoint(x: 605, y: 265)); folds.line(to: NSPoint(x: 605, y: 680)); folds.stroke()
        let arrow = NSBezierPath()
        arrow.move(to: NSPoint(x: 335, y: 405)); arrow.line(to: NSPoint(x: 690, y: 645))
        arrow.line(to: NSPoint(x: 600, y: 390)); arrow.line(to: NSPoint(x: 525, y: 490)); arrow.close()
        NSColor(calibratedRed: 0.94, green: 0.42, blue: 0.23, alpha: 1).setFill(); arrow.fill()
        image.unlockFocus()
        let bitmap = NSBitmapImageRep(data: image.tiffRepresentation!)!
        let suffix = scale == 2 ? "@2x" : ""
        try bitmap.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: "\(output)/icon_\(size)x\(size)\(suffix).png"))
    }
}
