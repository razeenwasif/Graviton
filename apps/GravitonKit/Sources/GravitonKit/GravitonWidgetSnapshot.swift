import Foundation
#if canImport(WidgetKit)
import WidgetKit
#endif

/// Lightweight summary of a container surfaced in Graviton's Home/Desktop widgets.
public struct WidgetContainerItem: Codable, Sendable, Identifiable, Hashable {
    public let id: String
    public let name: String
    public let image: String
    public let status: ContainerStatus
    public let cpuPercent: Double?
    public let memoryBytes: UInt64?
    public let portLabel: String?
    public let stack: String?

    public init(
        id: String,
        name: String,
        image: String,
        status: ContainerStatus,
        cpuPercent: Double? = nil,
        memoryBytes: UInt64? = nil,
        portLabel: String? = nil,
        stack: String? = nil
    ) {
        self.id = id
        self.name = name
        self.image = image
        self.status = status
        self.cpuPercent = cpuPercent
        self.memoryBytes = memoryBytes
        self.portLabel = portLabel
        self.stack = stack
    }
}

/// Summary snapshot shared between Graviton and its macOS/iOS WidgetKit extensions.
public struct GravitonWidgetSnapshot: Codable, Sendable, Equatable {
    public let updatedMs: UInt64
    public let hostName: String
    public let online: Bool
    public let isDemo: Bool
    public let runningCount: Int
    public let pausedCount: Int
    public let totalContainers: Int
    public let imageCount: Int
    public let volumeCount: Int
    public let networkCount: Int
    public let totalCPU: Double
    public let vmMemoryUsedBytes: UInt64
    public let hostMemoryBytes: UInt64
    public let vmMemoryFraction: Double
    public let otherHostMemoryFraction: Double
    public let containers: [WidgetContainerItem]

    public init(
        updatedMs: UInt64 = UInt64(Date().timeIntervalSince1970 * 1000),
        hostName: String,
        online: Bool,
        isDemo: Bool = false,
        runningCount: Int,
        pausedCount: Int = 0,
        totalContainers: Int,
        imageCount: Int,
        volumeCount: Int,
        networkCount: Int,
        totalCPU: Double,
        vmMemoryUsedBytes: UInt64,
        hostMemoryBytes: UInt64,
        vmMemoryFraction: Double,
        otherHostMemoryFraction: Double,
        containers: [WidgetContainerItem]
    ) {
        self.updatedMs = updatedMs
        self.hostName = hostName
        self.online = online
        self.isDemo = isDemo
        self.runningCount = runningCount
        self.pausedCount = pausedCount
        self.totalContainers = totalContainers
        self.imageCount = imageCount
        self.volumeCount = volumeCount
        self.networkCount = networkCount
        self.totalCPU = totalCPU
        self.vmMemoryUsedBytes = vmMemoryUsedBytes
        self.hostMemoryBytes = hostMemoryBytes
        self.vmMemoryFraction = vmMemoryFraction
        self.otherHostMemoryFraction = otherHostMemoryFraction
        self.containers = containers
    }

    public static let placeholder = GravitonWidgetSnapshot(
        hostName: "This Mac",
        online: true,
        isDemo: false,
        runningCount: 3,
        pausedCount: 0,
        totalContainers: 5,
        imageCount: 9,
        volumeCount: 4,
        networkCount: 2,
        totalCPU: 14.8,
        vmMemoryUsedBytes: 1_932_735_283,
        hostMemoryBytes: 34_359_738_368,
        vmMemoryFraction: 0.08,
        otherHostMemoryFraction: 0.42,
        containers: [
            WidgetContainerItem(id: "api-1", name: "api-gateway", image: "ghcr.io/app/api:latest", status: .running, cpuPercent: 8.4, memoryBytes: 612_000_000, portLabel: ":8080", stack: "core"),
            WidgetContainerItem(id: "pg-1", name: "postgres-17", image: "postgres:17-alpine", status: .running, cpuPercent: 4.1, memoryBytes: 840_000_000, portLabel: ":5432", stack: "core"),
            WidgetContainerItem(id: "rd-1", name: "redis-cache", image: "redis:7.4-alpine", status: .running, cpuPercent: 2.3, memoryBytes: 180_000_000, portLabel: ":6379", stack: "core"),
            WidgetContainerItem(id: "wk-1", name: "worker-queue", image: "python:3.13-slim", status: .exited, cpuPercent: nil, memoryBytes: nil, portLabel: nil, stack: nil),
        ]
    )
}

public enum GravitonWidgetStore {
    #if os(macOS)
    public static let widgetGroupID: String = {
        if let configured = Bundle.main.object(forInfoDictionaryKey: "GravitonWidgetGroupID") as? String,
           !configured.isEmpty,
           !configured.contains("$(") {
            return configured
        }
        return "group.dev.graviton.shared"
    }()
    #else
    public static let widgetGroupID = "group.dev.graviton.shared"
    #endif
    private static let filename = "widget-snapshot.json"

    public static func save(_ snapshot: GravitonWidgetSnapshot) {
        guard let url = snapshotURL() else { return }
        do {
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            let data = try JSONEncoder().encode(snapshot)
            try data.write(to: url, options: .atomic)
            #if canImport(WidgetKit)
            WidgetCenter.shared.reloadAllTimelines()
            #endif
        } catch {
            // Non-fatal: widget refreshes on next app cycle.
        }
    }

    public static func load() -> GravitonWidgetSnapshot? {
        guard let url = snapshotURL(),
              let data = try? Data(contentsOf: url) else {
            return nil
        }
        return try? JSONDecoder().decode(GravitonWidgetSnapshot.self, from: data)
    }

    private static func snapshotURL() -> URL? {
        if let container = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: widgetGroupID) {
            return container.appendingPathComponent(filename)
        }
        #if os(macOS)
        return FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Group Containers", isDirectory: true)
            .appendingPathComponent(widgetGroupID, isDirectory: true)
            .appendingPathComponent(filename)
        #else
        return nil
        #endif
    }
}
