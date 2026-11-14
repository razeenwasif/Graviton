import AppKit

/// The menu bar glyph: the Graviton mark redrawn for an 18 pt template image.
///
/// Same idea as the app icon (a container cube inside a tilted orbit, with a
/// particle), simplified so it reads at menu bar size. It is a template image,
/// so macOS tints it for light, dark and highlighted menu bars; the cube's
/// three faces use different opacities to keep its shading in one colour.
/// `design/menubar.svg` is the same geometry, for reference and exports.
enum MenuBarGlyph {
    /// Engine answering: solid, shaded cube.
    static let live = make(online: true)
    /// No engine (demo data, or gravitond not answering): hollow cube.
    static let offline = make(online: false)

    private static let size = NSSize(width: 18, height: 18)

    private static func make(online: Bool) -> NSImage {
        let image = NSImage(size: size, flipped: true) { _ in
            guard let ctx = NSGraphicsContext.current?.cgContext else { return false }
            draw(in: ctx, online: online)
            return true
        }
        image.isTemplate = true
        image.accessibilityDescription = online ? "Graviton" : "Graviton (engine not running)"
        return image
    }

    // Geometry in an 18 × 18 box, y down.
    private static let top = CGPoint(x: 9, y: 2.4)
    private static let upperLeft = CGPoint(x: 3.9, y: 5.35)
    private static let upperRight = CGPoint(x: 14.1, y: 5.35)
    private static let middle = CGPoint(x: 9, y: 8.3)
    private static let lowerLeft = CGPoint(x: 3.9, y: 11.3)
    private static let lowerRight = CGPoint(x: 14.1, y: 11.3)
    private static let bottom = CGPoint(x: 9, y: 14.25)

    private static let orbitCenter = CGPoint(x: 9, y: 10.4)
    private static let orbitRadii = CGSize(width: 8.1, height: 2.75)
    private static let tilt: CGFloat = -18 * .pi / 180
    private static let stroke: CGFloat = 1.25

    private static func polygon(_ points: CGPoint...) -> CGPath {
        let path = CGMutablePath()
        path.addLines(between: points)
        path.closeSubpath()
        return path
    }

    /// Rotated around the orbit's centre.
    private static var orbitTransform: CGAffineTransform {
        CGAffineTransform(translationX: orbitCenter.x, y: orbitCenter.y).rotated(by: tilt)
    }

    private static func draw(in ctx: CGContext, online: Bool) {
        let hexagon = polygon(top, upperRight, lowerRight, bottom, lowerLeft, upperLeft)
        var t = orbitTransform
        let orbit = CGPath(ellipseIn: CGRect(x: -orbitRadii.width, y: -orbitRadii.height,
                                             width: orbitRadii.width * 2, height: orbitRadii.height * 2),
                           transform: &t)
        // The orbit's front half is the side of its tilted axis nearer the viewer (below it).
        let frontHalf = CGPath(rect: CGRect(x: -20, y: 0, width: 40, height: 20), transform: &t)

        // 1. Back of the orbit, hidden where the cube stands in front of it.
        ctx.saveGState()
        ctx.addRect(CGRect(origin: .zero, size: size))
        ctx.addPath(frontHalf)
        ctx.addPath(hexagon)
        ctx.clip(using: .evenOdd)
        ctx.setAlpha(online ? 0.55 : 0.45)
        ctx.addPath(orbit)
        ctx.setLineWidth(stroke * 0.9)
        ctx.strokePath()
        ctx.restoreGState()

        // 2. The cube.
        if online {
            ctx.setFillColor(.black)
            for (face, alpha) in [(polygon(top, upperRight, middle, upperLeft), 1.0),
                                  (polygon(upperLeft, middle, bottom, lowerLeft), 0.62),
                                  (polygon(middle, upperRight, lowerRight, bottom), 0.34)] {
                ctx.setAlpha(alpha)
                ctx.addPath(face)
                ctx.fillPath()
            }
            ctx.setAlpha(1)
        } else {
            ctx.setStrokeColor(.black)
            ctx.setLineWidth(1.1)
            ctx.setLineJoin(.round)
            ctx.addPath(polygon(top, upperRight, lowerRight, bottom, lowerLeft, upperLeft))
            ctx.move(to: upperLeft); ctx.addLine(to: middle); ctx.addLine(to: upperRight)
            ctx.move(to: middle); ctx.addLine(to: bottom)
            ctx.strokePath()
        }

        // 3. Front of the orbit, with a hairline gap cut into the cube so it reads as passing in front.
        ctx.saveGState()
        ctx.addPath(frontHalf)
        ctx.clip()
        ctx.setBlendMode(.clear)
        ctx.addPath(orbit)
        ctx.setLineWidth(stroke + 1.1)
        ctx.strokePath()
        ctx.setBlendMode(.normal)
        ctx.setStrokeColor(.black)
        ctx.addPath(orbit)
        ctx.setLineWidth(stroke)
        ctx.strokePath()
        ctx.restoreGState()

        // 4. The particle riding the orbit at its front left, with a gap around it.
        let particle = CGPoint(x: -orbitRadii.width * 0.865, y: orbitRadii.height * 0.5).applying(orbitTransform)
        let r: CGFloat = 1.55
        ctx.setBlendMode(.clear)
        ctx.fillEllipse(in: CGRect(x: particle.x - r - 0.9, y: particle.y - r - 0.9, width: (r + 0.9) * 2, height: (r + 0.9) * 2))
        ctx.setBlendMode(.normal)
        ctx.setFillColor(.black)
        ctx.fillEllipse(in: CGRect(x: particle.x - r, y: particle.y - r, width: r * 2, height: r * 2))
    }
}
