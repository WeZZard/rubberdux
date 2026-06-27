import Foundation

// MARK: - SurfaceOp

/// A payload-bearing surface op the agent (or a human echo) applies to a
/// surface element. Mirrors `SurfaceOp` in `src/agent/world/surface.rs`, which
/// serde encodes internally tagged on the `op` field with `snake_case` variant
/// names (`set_value` / `click` / `navigate` / `render`). The optional
/// `base_version` is the optimistic-concurrency precondition; serde emits it as
/// an explicit `null` when absent. See docs/agent/world/ecs-runtime.md
/// (SurfaceOp).
enum SurfaceOp: Codable, Equatable {
    /// Set the value of a surface element. `value` is the new JSON-encoded
    /// element value.
    case setValue(surface: SurfaceId, element: ElementId, value: JSONValue, baseVersion: SurfaceVersion?)
    /// Synthesise a click on a surface element. `point` overrides the element's
    /// default hit-test point when present.
    case click(surface: SurfaceId, element: ElementId, point: SurfacePoint?, baseVersion: SurfaceVersion?)
    /// Navigate the surface to a new route.
    case navigate(surface: SurfaceId, route: Route, baseVersion: SurfaceVersion?)
    /// Render a component tree into a surface region.
    case render(surface: SurfaceId, component: ComponentSpec, baseVersion: SurfaceVersion?)

    /// The `SurfaceId` this op targets — every variant names a `surface`.
    var surface: SurfaceId {
        switch self {
        case .setValue(let surface, _, _, _): return surface
        case .click(let surface, _, _, _): return surface
        case .navigate(let surface, _, _): return surface
        case .render(let surface, _, _): return surface
        }
    }

    /// The optimistic-concurrency precondition: when non-nil the op applies only
    /// if the target surface's current `SurfaceVersion` equals this value.
    var baseVersion: SurfaceVersion? {
        switch self {
        case .setValue(_, _, _, let baseVersion): return baseVersion
        case .click(_, _, _, let baseVersion): return baseVersion
        case .navigate(_, _, let baseVersion): return baseVersion
        case .render(_, _, let baseVersion): return baseVersion
        }
    }

    private enum TagKey: String, CodingKey {
        case op
    }

    private enum SetValueKeys: String, CodingKey {
        case op
        case surface
        case element
        case value
        case baseVersion = "base_version"
    }

    private enum ClickKeys: String, CodingKey {
        case op
        case surface
        case element
        case point
        case baseVersion = "base_version"
    }

    private enum NavigateKeys: String, CodingKey {
        case op
        case surface
        case route
        case baseVersion = "base_version"
    }

    private enum RenderKeys: String, CodingKey {
        case op
        case surface
        case component
        case baseVersion = "base_version"
    }

    init(from decoder: Decoder) throws {
        let tagContainer = try decoder.container(keyedBy: TagKey.self)
        let tag = try tagContainer.decode(String.self, forKey: .op)
        switch tag {
        case "set_value":
            let c = try decoder.container(keyedBy: SetValueKeys.self)
            self = .setValue(
                surface: try c.decode(SurfaceId.self, forKey: .surface),
                element: try c.decode(ElementId.self, forKey: .element),
                value: try c.decode(JSONValue.self, forKey: .value),
                baseVersion: try c.decodeIfPresent(SurfaceVersion.self, forKey: .baseVersion)
            )
        case "click":
            let c = try decoder.container(keyedBy: ClickKeys.self)
            self = .click(
                surface: try c.decode(SurfaceId.self, forKey: .surface),
                element: try c.decode(ElementId.self, forKey: .element),
                point: try c.decodeIfPresent(SurfacePoint.self, forKey: .point),
                baseVersion: try c.decodeIfPresent(SurfaceVersion.self, forKey: .baseVersion)
            )
        case "navigate":
            let c = try decoder.container(keyedBy: NavigateKeys.self)
            self = .navigate(
                surface: try c.decode(SurfaceId.self, forKey: .surface),
                route: try c.decode(Route.self, forKey: .route),
                baseVersion: try c.decodeIfPresent(SurfaceVersion.self, forKey: .baseVersion)
            )
        case "render":
            let c = try decoder.container(keyedBy: RenderKeys.self)
            self = .render(
                surface: try c.decode(SurfaceId.self, forKey: .surface),
                component: try c.decode(ComponentSpec.self, forKey: .component),
                baseVersion: try c.decodeIfPresent(SurfaceVersion.self, forKey: .baseVersion)
            )
        default:
            throw DecodingError.dataCorruptedError(
                forKey: TagKey.op,
                in: tagContainer,
                debugDescription: "Unknown SurfaceOp tag: \(tag)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        // serde emits the Option `base_version` (and `point`) as explicit `null`
        // when absent, so encode the optionals (not encodeIfPresent).
        switch self {
        case .setValue(let surface, let element, let value, let baseVersion):
            var c = encoder.container(keyedBy: SetValueKeys.self)
            try c.encode("set_value", forKey: .op)
            try c.encode(surface, forKey: .surface)
            try c.encode(element, forKey: .element)
            try c.encode(value, forKey: .value)
            try c.encode(baseVersion, forKey: .baseVersion)
        case .click(let surface, let element, let point, let baseVersion):
            var c = encoder.container(keyedBy: ClickKeys.self)
            try c.encode("click", forKey: .op)
            try c.encode(surface, forKey: .surface)
            try c.encode(element, forKey: .element)
            try c.encode(point, forKey: .point)
            try c.encode(baseVersion, forKey: .baseVersion)
        case .navigate(let surface, let route, let baseVersion):
            var c = encoder.container(keyedBy: NavigateKeys.self)
            try c.encode("navigate", forKey: .op)
            try c.encode(surface, forKey: .surface)
            try c.encode(route, forKey: .route)
            try c.encode(baseVersion, forKey: .baseVersion)
        case .render(let surface, let component, let baseVersion):
            var c = encoder.container(keyedBy: RenderKeys.self)
            try c.encode("render", forKey: .op)
            try c.encode(surface, forKey: .surface)
            try c.encode(component, forKey: .component)
            try c.encode(baseVersion, forKey: .baseVersion)
        }
    }
}
