// swift-tools-version: 5.9
import PackageDescription
let package = Package(name: "TravelCompanion", platforms: [.macOS(.v14)], products: [.executable(name: "TravelCompanion", targets: ["TravelCompanion"])], targets: [.executableTarget(name: "TravelCompanion")])
