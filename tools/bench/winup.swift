import CoreGraphics
// Prints the window number of PID's first on-screen, normal-layer window wider than 400 pt.
let pid = Int(CommandLine.arguments[1])!
let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as! [[String: Any]]
for w in list {
  guard (w[kCGWindowOwnerPID as String] as? Int) == pid, (w[kCGWindowLayer as String] as? Int) == 0 else { continue }
  let b = w[kCGWindowBounds as String] as? [String: Any] ?? [:]
  if let wd = b["Width"] as? Double, wd > 400 { print(w[kCGWindowNumber as String]!); break }
}
