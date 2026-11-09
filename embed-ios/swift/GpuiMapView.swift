// The Swift glue for gpui-maps' embeddable engine: copy this file into the
// app target. `MapEngine`, `EdgeInsets`, `TouchPhase` and the delegate
// protocol come from the uniffi-generated `gpui_maps_embed.swift` (add
// `import <Module>` here if those bindings live in another module).
import QuartzCore
import SwiftUI
import UIKit

/// A `UIView` backed by a `CAMetalLayer` that the engine renders into.
///
/// The view owns the three things the engine asks a host for: the layer,
/// a `CADisplayLink` that runs only while the engine says a frame is owed,
/// and the touches, forwarded in points.
final class MetalMapView: UIView {
    override class var layerClass: AnyClass { CAMetalLayer.self }
    private var metalLayer: CAMetalLayer { layer as! CAMetalLayer }

    private(set) var engine: MapEngine?
    private var link: CADisplayLink?
    private var wakeScheduled = false
    /// Called once the engine exists, to add the app's layers.
    var onReady: ((MapEngine) -> Void)?
    var onTap: ((String, String) -> Void)?
    /// The camera after a frame that moved it (a gesture, a glide, an
    /// ease): on the main queue, after the engine's call has returned.
    var onCameraChanged: ((CameraState) -> Void)?
    /// What the app floats over the map beyond the safe area (a bottom
    /// sheet's visible height, a search bar), in points. Cheap to set on
    /// every frame of a sheet drag.
    var extraInsets: UIEdgeInsets = .zero {
        didSet { if extraInsets != oldValue { pushInsets() } }
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        backgroundColor = .clear
    }

    required init?(coder: NSCoder) { fatalError("not from a nib") }

    deinit { link?.invalidate() }

    private var pixelSize: (UInt32, UInt32, Double) {
        let scale = window?.screen.scale ?? UIScreen.main.scale
        return (UInt32((bounds.width * scale).rounded()), UInt32((bounds.height * scale).rounded()), Double(scale))
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        guard bounds.width > 0, bounds.height > 0, window != nil else { return }
        let (w, h, scale) = pixelSize
        metalLayer.contentsScale = CGFloat(scale)
        if let engine {
            engine.resize(width: w, height: h, scale: scale)
        } else {
            createEngine(width: w, height: h, scale: scale)
        }
        pushInsets()
        // RTL mirrors the engine's compass to the top-left.
        engine?.setLayoutDirection(rtl: effectiveUserInterfaceLayoutDirection == .rightToLeft)
        wake()
    }

    override func safeAreaInsetsDidChange() {
        super.safeAreaInsetsDidChange()
        pushInsets()
    }

    /// The safe area plus `extraInsets`: where the engine keeps its credits
    /// and compass, and centres camera moves.
    private func pushInsets() {
        let s = safeAreaInsets, e = extraInsets
        engine?.setInsets(insets: EdgeInsets(
            top: Double(s.top + e.top), bottom: Double(max(s.bottom, e.bottom)),
            left: Double(s.left + e.left), right: Double(s.right + e.right)))
    }

    private func createEngine(width: UInt32, height: UInt32, scale: Double) {
        let pointer = UInt64(UInt(bitPattern: Unmanaged.passUnretained(metalLayer).toOpaque()))
        let caches = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first?.path ?? NSTemporaryDirectory()
        do {
            let engine = try MapEngine(layerPtr: pointer, width: width, height: height, scale: scale, cacheDir: caches)
            engine.setDelegate(delegate: Delegate(view: self))
            self.engine = engine
            print("[sample] engine up: \(width)x\(height) px at \(scale)x; basemaps \(engine.basemaps())")
            print("[sample] credits: \(engine.attributions())")
            let link = CADisplayLink(target: LinkTarget(view: self), selector: #selector(LinkTarget.tick(_:)))
            // The screen's maximum (120 on ProMotion, with
            // CADisableMinimumFrameDurationOnPhone in Info.plist).
            let maxFps = Float(window?.screen.maximumFramesPerSecond ?? 60)
            link.preferredFrameRateRange = CAFrameRateRange(minimum: 30, maximum: maxFps, preferred: maxFps)
            link.add(to: .main, forMode: .common)
            self.link = link
            onReady?(engine)
        } catch {
            print("[sample] engine failed: \(error)")
        }
    }

    /// Unpauses the display link: a frame is owed.
    func wake() {
        link?.isPaused = false
    }

    fileprivate func tick(_ link: CADisplayLink) {
        guard let engine else { return }
        switch engine.render(t: link.targetTimestamp) {
        case .again:
            break
        case .idle:
            link.isPaused = true
        case let .at(t):
            link.isPaused = true
            guard !wakeScheduled else { return }
            wakeScheduled = true
            let delay = max(0, t - CACurrentMediaTime())
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in
                self?.wakeScheduled = false
                self?.wake()
            }
        }
    }

    // MARK: touches, in points

    private func forward(_ touches: Set<UITouch>, _ phase: TouchPhase) {
        guard let engine else { return }
        for touch in touches {
            let p = touch.location(in: self)
            let id = UInt64(UInt(bitPattern: ObjectIdentifier(touch).hashValue))
            engine.touch(phase: phase, id: id, x: Double(p.x), y: Double(p.y), t: touch.timestamp)
        }
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) { forward(touches, .began) }
    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) { forward(touches, .moved) }
    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) { forward(touches, .ended) }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) { forward(touches, .cancelled) }
}

/// The display link's target, so the link does not retain the view.
private final class LinkTarget {
    weak var view: MetalMapView?
    init(view: MetalMapView) { self.view = view }
    @objc func tick(_ link: CADisplayLink) {
        guard let view else { link.invalidate(); return }
        view.tick(link)
    }
}

/// The engine's delegate. Weak to the view: the engine holds it strongly.
private final class Delegate: MapEngineDelegate {
    weak var view: MetalMapView?
    init(view: MetalMapView) { self.view = view }

    func onTap(layer: String, id: String) {
        view?.onTap?(layer, id)
    }

    /// Hopped to the main queue so a handler may call back into the engine
    /// (e.g. re-upsert markers sized for the new zoom).
    func onCameraChanged(camera: CameraState) {
        DispatchQueue.main.async { [weak view] in view?.onCameraChanged?(camera) }
    }

    /// Any thread: hop to main, and do not call back into the engine here.
    func onNeedsFrame() {
        DispatchQueue.main.async { [weak view] in view?.wake() }
    }
}

/// The SwiftUI face of `MetalMapView`.
struct MapViewRepresentable: UIViewRepresentable {
    var onReady: (MapEngine) -> Void
    var onTap: (String, String) -> Void
    /// See `MetalMapView.onCameraChanged`.
    var onCameraChanged: ((CameraState) -> Void)? = nil
    /// See `MetalMapView.extraInsets`: e.g. the bottom sheet's height.
    var extraInsets: UIEdgeInsets = .zero

    func makeUIView(context: Context) -> MetalMapView {
        let view = MetalMapView(frame: .zero)
        view.onReady = onReady
        view.onTap = onTap
        view.onCameraChanged = onCameraChanged
        return view
    }

    func updateUIView(_ view: MetalMapView, context: Context) {
        view.onTap = onTap
        view.onCameraChanged = onCameraChanged
        view.extraInsets = extraInsets
    }
}
