import SwiftUI

@main
struct SampleApp: App {
    var body: some Scene {
        WindowGroup { ContentView() }
    }
}

/// Singapore: a few stops, one route, three buses moving along it by id
/// every second, and taps logged. Launch with `-rtl` to lay the host's own
/// chrome out right-to-left (the engine's chrome flag is a later task).
struct ContentView: View {
    @State private var lastTap = "Tap a stop, the route or a bus"
    @StateObject private var fleet = Fleet()
    private let rtl = ProcessInfo.processInfo.arguments.contains("-rtl")

    var body: some View {
        ZStack(alignment: .top) {
            MapViewRepresentable(
                onReady: { engine in fleet.start(engine) },
                onTap: { layer, id in lastTap = "\(layer): \(id)" },
                // Buses are sized for the zoom: redraw them when it moves.
                onCameraChanged: { _ in fleet.refresh() }
            )
            .ignoresSafeArea()
            HStack {
                Text(lastTap).font(.callout.weight(.medium))
                Spacer(minLength: 0)
                Text(rtl ? "RTL" : "LTR").font(.caption.monospaced()).foregroundStyle(.secondary)
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
            .padding(.horizontal, 16)
            .padding(.top, 8)
        }
        .environment(\.layoutDirection, rtl ? .rightToLeft : .leftToRight)
    }
}

/// The demo data, and the timer that moves the buses.
@MainActor
final class Fleet: ObservableObject {
    struct Stop { let id: String; let name: String; let lon: Double; let lat: Double }

    /// Around Marina Bay, in route order.
    let stops = [
        Stop(id: "S01", name: "Raffles Place", lon: 103.8514, lat: 1.2840),
        Stop(id: "S02", name: "Fullerton", lon: 103.8532, lat: 1.2864),
        Stop(id: "S03", name: "Esplanade", lon: 103.8557, lat: 1.2896),
        Stop(id: "S04", name: "Suntec", lon: 103.8588, lat: 1.2948),
        Stop(id: "S05", name: "Promenade", lon: 103.8612, lat: 1.2932),
        Stop(id: "S06", name: "Bayfront", lon: 103.8600, lat: 1.2817),
    ]
    private var route: [LngLat] { stops.map { LngLat(lon: $0.lon, lat: $0.lat) } }
    private var timer: Timer?
    private var tick = 0.0
    private weak var engine: MapEngine?

    func start(_ engine: MapEngine) {
        self.engine = engine
        engine.jumpTo(update: CameraUpdate(lon: 103.8562, lat: 1.2885, zoom: 14.6))
        do {
            try engine.setStyle(layer: "route", paintJson: ##"{"line-width": 5, "line-color": "#0a84ff"}"##)
            try engine.upsertLines(layer: "route", features: [LineFeature(id: "R-10", coords: route, props: [:])])
            try engine.setStyle(layer: "stops", paintJson: #"{"circle-radius": 6, "circle-color": ["get", "color"]}"#)
            try engine.upsertPoints(layer: "stops", features: stops.map {
                PointFeature(id: $0.id, lon: $0.lon, lat: $0.lat, props: [
                    "name": .text(value: $0.name),
                    "color": .text(value: "#1c1c1e"),
                ])
            })
            try engine.setStyle(layer: "buses", paintJson: ##"{"fill-color": "#ff3b30", "fill-opacity": 1}"##)
        } catch {
            print("[sample] layers refused: \(error)")
        }
        moveBuses()
        timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.moveBuses() }
        }
    }

    /// Three buses spaced along the route, each upserted by id with a
    /// heading-shaped marker sized for the current zoom.
    private func moveBuses() {
        tick += 1
        refresh()
    }

    /// The buses at the current tick, re-sized for the current zoom.
    func refresh() {
        guard let engine else { return }
        let zoom = engine.camera().zoom
        let buses = (0..<3).map { i -> PolygonFeature in
            let (p, heading) = along(route, fraction: ((tick * 0.01) + Double(i) / 3).truncatingRemainder(dividingBy: 1))
            let ring = headingMarker(lon: p.lon, lat: p.lat, headingDeg: heading, zoom: zoom)
            return PolygonFeature(id: "bus-\(i + 1)", rings: [ring], props: ["route": .text(value: "R-10")])
        }
        do {
            try engine.upsertPolygons(layer: "buses", features: buses)
        } catch {
            print("[sample] buses refused: \(error)")
        }
    }

    /// The point `fraction` of the way along `line` (by vertex-to-vertex
    /// length, degrees), and the compass heading there.
    private func along(_ line: [LngLat], fraction: Double) -> (LngLat, Double) {
        let lengths = zip(line, line.dropFirst()).map { hypot($1.lon - $0.lon, $1.lat - $0.lat) }
        var left = lengths.reduce(0, +) * fraction
        for (i, length) in lengths.enumerated() {
            if left <= length || i == lengths.count - 1 {
                let (a, b) = (line[i], line[i + 1])
                let k = length > 0 ? min(1, left / length) : 0
                let p = LngLat(lon: a.lon + (b.lon - a.lon) * k, lat: a.lat + (b.lat - a.lat) * k)
                let heading = atan2(b.lon - a.lon, b.lat - a.lat) * 180 / .pi
                return (p, heading)
            }
            left -= length
        }
        return (line[0], 0)
    }
}
