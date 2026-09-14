// Ask another process to publish its web content into the accessibility tree.
//
//   swiftc -O tools/mac-ax-enable.swift -o /tmp/ax-enable
//   /tmp/ax-enable <pid>
//
// **Why this exists.** A Tauri window on macOS is a WKWebView, and WebKit builds
// the DOM's accessibility tree *lazily*: until something asks, `entire contents of
// front window` returns nothing at all, which is exactly what the first click probe
// measured. Chrome, Firefox and Electron all behave the same way — they skip the
// work until a screen reader turns up.
//
// `AXManualAccessibility` is the attribute assistive technology sets to say it has
// turned up. It is set on *another* application's element, from outside, which is
// the whole point here: it needs no change to the app under test, so what gets
// driven is the binary that ships rather than a special build of it.
//
// `AXEnhancedUserInterface` is set as well because the two are easy to confuse and
// only one of them is likely to matter. Both results are printed so the run says
// which. It is the older, blunter switch and is known to upset window managers
// (bugzilla.mozilla.org/show_bug.cgi?id=1664992), so if `AXManualAccessibility`
// turns out to be sufficient, drop it.
//
// Requires Accessibility permission for whatever runs it — on a GitHub macOS runner
// that is already granted, which the second probe established by reading a window's
// frame out of the tree.

import ApplicationServices
import Foundation

let args = CommandLine.arguments
guard args.count > 1, let pid = Int32(args[1]) else {
    FileHandle.standardError.write("usage: ax-enable <pid>\n".data(using: .utf8)!)
    exit(2)
}

// Trusted first, and reported rather than assumed: without Accessibility every set
// below returns `cannotComplete`, which reads like a WebKit problem rather than a
// permission one. `false` for the prompt — nothing can answer a dialog here.
let trusted = AXIsProcessTrustedWithOptions(
    [kAXTrustedCheckOptionPrompt.takeUnretainedValue(): false] as CFDictionary
)
print("accessibility trusted: \(trusted)")

let app = AXUIElementCreateApplication(pid)

func set(_ attribute: String) {
    let err = AXUIElementSetAttributeValue(app, attribute as CFString, kCFBooleanTrue)
    // 0 is `.success`. Anything else is printed as its raw value, because the
    // interesting failures (`cannotComplete`, `attributeUnsupported`) mean quite
    // different things and a bool would hide which.
    print("\(attribute): \(err.rawValue)\(err == .success ? " (ok)" : "")")
}

set("AXManualAccessibility")
set("AXEnhancedUserInterface")
