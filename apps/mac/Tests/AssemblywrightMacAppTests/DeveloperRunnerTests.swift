import Foundation
import Testing
@testable import AssemblywrightMacApp

@Suite("Developer runner live observation")
struct DeveloperRunnerTests {
  @Test(.enabled(if: ProcessInfo.processInfo.environment["ASSEMBLYWRIGHT_DEVELOPER_LIVE_CONFIG"] != nil))
  @MainActor
  func swiftModelObservesWindowsRunner() async throws {
    let path = try #require(ProcessInfo.processInfo.environment["ASSEMBLYWRIGHT_DEVELOPER_LIVE_CONFIG"])
    let model = DeveloperRunnerModel(configurationPath: path)
    let observation = Task { await model.observe() }
    defer { observation.cancel() }
    for _ in 0..<100 {
      if model.snapshot != nil { break }
      try await Task.sleep(for: .milliseconds(100))
    }
    let snapshot = try #require(model.snapshot)
    #expect(model.error == nil)
    #expect(snapshot.mode == "supervised_developer")
    #expect(!snapshot.host.isEmpty)
    #expect(snapshot.queue.allSatisfy { UUID(uuidString: $0.id) != nil })
    #expect(snapshot.queue.allSatisfy {
      ["queued", "running", "paused", "failed", "succeeded"].contains($0.status)
    })
  }
}
