import Foundation
import SwiftUI

struct DeveloperRunnerConfiguration: Decodable {
  let endpoint: String
  let token: String
}
struct DeveloperRunnerFeature: Decodable, Identifiable {
  let id: String
  let project: String
  let instruction: String
  let validation: String
  let status: String
  let checkpoint: String
  let message: String
  let changedFiles: [String]
}
struct DeveloperRunnerSnapshot: Decodable {
  let mode: String
  let host: String
  let workspaceRoot: String
  let revision: UInt64
  let autoRun: Bool
  let emergencyPaused: Bool
  let running: Bool
  let queue: [DeveloperRunnerFeature]
}

@MainActor
final class DeveloperRunnerModel: ObservableObject {
  @Published var snapshot: DeveloperRunnerSnapshot?
  @Published var error: String?
  @Published var sending = false
  private let configuration: DeveloperRunnerConfiguration?
  private let session: URLSession

  init(configurationPath: String, session: URLSession? = nil) {
    configuration = try? JSONDecoder().decode(
      DeveloperRunnerConfiguration.self,
      from: Data(contentsOf: URL(fileURLWithPath: configurationPath)))
    let settings = URLSessionConfiguration.ephemeral
    settings.timeoutIntervalForRequest = 10
    self.session = session ?? URLSession(configuration: settings)
  }

  private func request(path: String, body: [String: Any]? = nil) async throws
    -> DeveloperRunnerSnapshot
  {
    guard let configuration, let base = URL(string: configuration.endpoint),
      ["127.0.0.1", "localhost", "::1"].contains(base.host ?? ""), base.scheme == "http"
    else {
      throw NSError(
        domain: "Developer runner", code: 1,
        userInfo: [
          NSLocalizedDescriptionKey:
            "Open the developer build with its launcher to connect to Windows."
        ])
    }
    var request = URLRequest(url: base.appendingPathComponent(path))
    request.setValue("Bearer \(configuration.token)", forHTTPHeaderField: "Authorization")
    if let body {
      request.httpMethod = "POST"
      request.setValue("application/json", forHTTPHeaderField: "Content-Type")
      request.httpBody = try JSONSerialization.data(withJSONObject: body)
    }
    let (data, response) = try await session.data(for: request)
    guard let response = response as? HTTPURLResponse, response.statusCode == 200 else {
      let detail = (try? JSONSerialization.jsonObject(with: data)) as? [String: String]
      throw NSError(
        domain: "Developer runner", code: 2,
        userInfo: [NSLocalizedDescriptionKey: detail?["error"] ?? "Windows runner is unavailable."])
    }
    let decoder = JSONDecoder()
    decoder.keyDecodingStrategy = .convertFromSnakeCase
    let snapshot = try decoder.decode(DeveloperRunnerSnapshot.self, from: data)
    guard snapshot.mode == "supervised_developer" else { throw URLError(.cannotParseResponse) }
    return snapshot
  }

  func refresh() async {
    do {
      let observed = try await request(path: "status")
      if observed.revision >= (snapshot?.revision ?? 0) { snapshot = observed }
      error = nil
    } catch { self.error = error.localizedDescription }
  }

  func observe() async {
    while !Task.isCancelled {
      await refresh()
      try? await Task.sleep(for: .milliseconds(800))
    }
  }

  func send(_ action: String, values: [String: Any] = [:]) async {
    if sending && action != "emergency" && action != "stop" { return }
    sending = true
    defer { sending = false }
    do {
      var body = values
      body["action"] = action
      let updated = try await request(path: "control", body: body)
      if updated.revision >= (snapshot?.revision ?? 0) { snapshot = updated }
      error = nil
    } catch { self.error = error.localizedDescription }
  }
}

struct DeveloperRunnerView: View {
  @StateObject private var model: DeveloperRunnerModel
  @State private var project = "first-project"
  @State private var instruction = ""
  @State private var validation = "python -m unittest discover -s tests -v"
  @State private var confirmingStart = false
  @State private var enqueueID = UUID().uuidString.lowercased()

  init(configurationPath: String) {
    _model = StateObject(wrappedValue: DeveloperRunnerModel(configurationPath: configurationPath))
  }
  private var next: DeveloperRunnerFeature? {
    model.snapshot?.queue.first { $0.status != "succeeded" }
  }
  private var startLabel: String {
    if let next, ["paused", "failed"].contains(next.status) { return "Resume" }
    return (model.snapshot?.queue.contains { $0.status == "succeeded" } ?? false)
      ? "Start next feature" : "Start"
  }
  var body: some View {
    ScrollView {
      VStack(alignment: .leading, spacing: 22) {
        HStack(alignment: .firstTextBaseline) {
          Text("Build with Assemblywright").font(.largeTitle.bold())
          Spacer()
          Text("Developer build").font(.caption.bold()).padding(7).background(
            .orange.opacity(0.15), in: Capsule())
        }
        if let snapshot = model.snapshot {
          Label("Connected to \(snapshot.host)", systemImage: "checkmark.circle.fill")
            .foregroundStyle(.green)
          Text(
            "Windows runs your project. The local model prepares changes; your validation command checks them."
          ).foregroundStyle(.secondary)
        } else {
          ProgressView("Connecting to Windows…")
        }
        if let error = model.error { Text(error).foregroundStyle(.red).textSelection(.enabled) }
        GroupBox("Add a feature") {
          VStack(alignment: .leading, spacing: 12) {
            TextField("Project folder on Windows", text: $project)
            TextField("What should this feature do?", text: $instruction, axis: .vertical)
              .lineLimit(3...8)
            TextField("Validation command", text: $validation)
            HStack {
              Button("Add to queue") {
                let values: [String: Any] = [
                  "id": enqueueID, "project": project, "instruction": instruction,
                  "validation": validation,
                ]
                Task {
                  await model.send("enqueue", values: values)
                  if model.error == nil {
                    instruction = ""
                    enqueueID = UUID().uuidString.lowercased()
                  }
                }
              }.disabled(
                model.sending || instruction.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                  || project.isEmpty || validation.isEmpty)
              Spacer()
              if let root = model.snapshot?.workspaceRoot {
                Text(root).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
              }
            }
          }.padding(10).textFieldStyle(.roundedBorder)
        }
        GroupBox("Assembly line") {
          VStack(alignment: .leading, spacing: 14) {
            HStack {
              Toggle(
                "Auto-run next feature",
                isOn: Binding(
                  get: { model.snapshot?.autoRun ?? true },
                  set: { enabled in
                    Task { await model.send("auto_run", values: ["enabled": enabled]) }
                  })
              )
              .disabled(model.snapshot == nil)
              Spacer()
              if model.snapshot?.running == true {
                ProgressView().controlSize(.small)
                Text("Running")
              } else if model.snapshot?.emergencyPaused == true {
                Text("Emergency paused").foregroundStyle(.orange)
              } else {
                Text(next == nil ? "Ready" : "Waiting for you").foregroundStyle(.secondary)
              }
            }
            HStack {
              Button(startLabel) { confirmingStart = true }
                .buttonStyle(.borderedProminent)
                .disabled(
                  next == nil || model.snapshot?.running == true
                    || model.snapshot?.emergencyPaused == true || model.sending)
              Button("Stop") { Task { await model.send("stop") } }.disabled(
                model.snapshot?.running != true)
              Button("Emergency Pause", role: .destructive) {
                Task { await model.send("emergency") }
              }.disabled(model.snapshot == nil)
              if model.snapshot?.emergencyPaused == true {
                Button("Clear Emergency Pause") { Task { await model.send("clear_emergency") } }
                  .disabled(model.snapshot?.running == true)
              }
            }
            Text(
              "Runs under your Windows account. Review the project and validation command before starting. Changes remain in the project folder for review."
            )
            .font(.caption).foregroundStyle(.secondary)
            Divider()
            if model.snapshot?.queue.isEmpty != false {
              Text("Add your first feature to begin.").foregroundStyle(.secondary)
            }
            ForEach(model.snapshot?.queue ?? []) { feature in
              VStack(alignment: .leading, spacing: 6) {
                HStack {
                  Text(feature.project).font(.headline)
                  Spacer()
                  Text(feature.status.replacingOccurrences(of: "_", with: " ").capitalized)
                    .foregroundStyle(
                      feature.status == "succeeded"
                        ? .green : feature.status == "failed" ? .red : .primary)
                }
                Text(feature.instruction)
                Text("Checkpoint: \(feature.checkpoint.replacingOccurrences(of: "_", with: " "))")
                  .font(.caption).foregroundStyle(.secondary)
                Text(feature.message).font(.caption).textSelection(.enabled)
                if !feature.changedFiles.isEmpty {
                  Text(feature.changedFiles.joined(separator: " · ")).font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                }
              }.padding(10).frame(maxWidth: .infinity, alignment: .leading).background(
                .quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 8))
            }
          }.padding(10)
        }
      }.padding(28)
    }.frame(minWidth: 840, minHeight: 680)
      .task { await model.observe() }
      .confirmationDialog(
        "Run the queued work on Windows?", isPresented: $confirmingStart, titleVisibility: .visible
      ) {
        Button(startLabel) {
          Task { await model.send(startLabel == "Resume" ? "resume" : "start") }
        }
        Button("Cancel", role: .cancel) {}
      } message: {
        Text(
          "The local model can change files in the selected project, and Windows will run the validation command. Auto-run continues through the features currently queued after validation passes. Features added later wait for another Start."
        )
      }
  }
}
