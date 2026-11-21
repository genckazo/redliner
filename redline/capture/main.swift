// redline-capture: screenshot one window with Redline's ink on top, and nothing else.
//
//   redline-capture --out shot.png --overlay 123[,456] [--points "x,y;x,y"]
//
// --points are the centres of the user's marks in global screen points (top-left origin).
// The window to capture is the one all of them sit on, or the frontmost window when there
// are none. --overlay lists Redline's own overlay windows (CGWindowIDs): they are skipped
// when looking for the window, and included in the picture so the ink shows.
//
// On success prints one tab-separated line and exits 0:
//   window <x> <y> <w> <h> <app> <title>        (the window's frame in global points)
// Exits 3 when no single window fits (marks on several windows or on none): the caller
// should capture the whole screen instead. Any other failure exits 1.
import CoreGraphics
import Foundation
import ImageIO
import ScreenCaptureKit
import UniformTypeIdentifiers

func fail(_ message: String, code: Int32 = 1) -> Never {
	FileHandle.standardError.write((message + "\n").data(using: .utf8)!)
	exit(code)
}

var out: String?
var overlayIDs = Set<CGWindowID>()
var points: [CGPoint] = []
var args = Array(CommandLine.arguments.dropFirst())
while !args.isEmpty {
	let flag = args.removeFirst()
	guard !args.isEmpty else { fail("missing value for \(flag)") }
	let value = args.removeFirst()
	switch flag {
	case "--out": out = value
	case "--overlay": overlayIDs = Set(value.split(separator: ",").compactMap { CGWindowID($0) })
	case "--points":
		points = value.split(separator: ";").compactMap { pair in
			let xy = pair.split(separator: ",").compactMap { Double($0) }
			return xy.count == 2 ? CGPoint(x: xy[0], y: xy[1]) : nil
		}
	default: fail("unknown flag \(flag)")
	}
}
guard let out else { fail("--out is required") }

struct Candidate {
	let id: CGWindowID
	let app: String
	let title: String
	let bounds: CGRect
}

// On-screen windows, front to back. Layer 0 is ordinary app windows (no menu bar, Dock, overlays).
let info = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
let candidates: [Candidate] = info.compactMap { w in
	guard let id = w[kCGWindowNumber as String] as? CGWindowID, !overlayIDs.contains(id),
		(w[kCGWindowLayer as String] as? Int) == 0,
		(w[kCGWindowAlpha as String] as? Double ?? 1) > 0,
		let dict = w[kCGWindowBounds as String] as? NSDictionary,
		let bounds = CGRect(dictionaryRepresentation: dict), bounds.width >= 60, bounds.height >= 60
	else { return nil }
	return Candidate(id: id, app: w[kCGWindowOwnerName as String] as? String ?? "", title: w[kCGWindowName as String] as? String ?? "", bounds: bounds)
}

let target: Candidate
if points.isEmpty {
	guard let front = candidates.first else { fail("no window on screen", code: 3) }
	target = front
} else {
	let hits = points.map { p in candidates.first { $0.bounds.contains(p) } }
	guard let first = hits[0], hits.allSatisfy({ $0?.id == first.id }) else { fail("marks are not all on one window", code: 3) }
	target = first
}

do {
	let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
	guard let window = content.windows.first(where: { $0.windowID == target.id }) else { fail("window is not shareable", code: 3) }
	let overlays = content.windows.filter { overlayIDs.contains($0.windowID) }
	guard let display = content.displays.first(where: { $0.frame.intersects(target.bounds) }) ?? content.displays.first else { fail("no display") }

	// Only the window and the ink are drawn; everything else on the display is left out.
	let filter = SCContentFilter(display: display, including: [window] + overlays)
	let rect = target.bounds.intersection(display.frame)
	let config = SCStreamConfiguration()
	config.sourceRect = CGRect(x: rect.minX - display.frame.minX, y: rect.minY - display.frame.minY, width: rect.width, height: rect.height)
	let scale = CGFloat(filter.pointPixelScale)
	config.width = Int(rect.width * scale)
	config.height = Int(rect.height * scale)
	config.showsCursor = false

	let image: CGImage = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: config)
	guard let destination = CGImageDestinationCreateWithURL(URL(fileURLWithPath: out) as CFURL, UTType.png.identifier as CFString, 1, nil) else { fail("cannot write \(out)") }
	CGImageDestinationAddImage(destination, image, nil)
	guard CGImageDestinationFinalize(destination) else { fail("cannot write \(out)") }

	let clean = { (text: String) in text.replacingOccurrences(of: "\t", with: " ").replacingOccurrences(of: "\n", with: " ") }
	print("window\t\(Int(rect.minX))\t\(Int(rect.minY))\t\(Int(rect.width))\t\(Int(rect.height))\t\(clean(target.app))\t\(clean(target.title))")
} catch {
	fail("capture failed: \(error.localizedDescription)")
}
