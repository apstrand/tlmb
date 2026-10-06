import SwiftUI
import UniformTypeIdentifiers

/// Bringing another device's accounts across (§8.3).
///
/// The export file, not §8.2's link or relay code: those need a socket, a PAKE
/// round trip and a confirmation on both ends, where the file is bytes and a
/// passphrase and so is the one route a phone has today. The container is text,
/// so it can also be pasted — which is what someone who sent it to themselves
/// in a message will try first.
struct TransferView: View {
    @Environment(MailboxStore.self) private var store

    @State private var export = ""
    @State private var fileName: String?
    @State private var passphrase = ""
    @State private var picking = false
    @State private var working = false
    @State private var failure: String?
    @State private var pasting = false

    private var ready: Bool {
        !working && !export.isEmpty && !passphrase.isEmpty
    }

    var body: some View {
        Form {
            Section {
                VStack(alignment: .leading, spacing: 8) {
                    Text("On the computer that already has your accounts, run:")
                    Text("mailbox pair --export-file mailbox.pair")
                        .font(.footnote.monospaced())
                        .textSelection(.enabled)
                        .padding(8)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(.quaternary, in: RoundedRectangle(cornerRadius: 6))
                    Text("It asks for a passphrase. Send the file here — AirDrop, Files, a message to yourself — then open it below and type the same passphrase.")
                        .foregroundStyle(.secondary)
                }
                .font(.subheadline)
            } header: {
                Text("From another device")
            }

            Section {
                Button {
                    picking = true
                } label: {
                    Label(fileName ?? "Choose the export", systemImage: "doc")
                }
                Button {
                    pasting = true
                } label: {
                    Label("Paste it instead", systemImage: "doc.on.clipboard")
                }
                SecureField(
                    "Passphrase",
                    text: $passphrase,
                    prompt: Text(verbatim: "Passphrase")
                )
                .labelsHidden()
                .textContentType(.password)
            } footer: {
                if let failure {
                    Text(failure).foregroundStyle(.red)
                } else if export.isEmpty {
                    Text("The file is a single block of text; this screen takes either it or its contents.")
                } else {
                    Text("Ready — \(export.count) characters of sealed accounts.")
                }
            }

            Section {
                Button {
                    Task { await load() }
                } label: {
                    HStack {
                        Spacer()
                        if working { ProgressView() } else { Text("Add these accounts") }
                        Spacer()
                    }
                }
                .disabled(!ready)
            } footer: {
                Text("Passwords go straight to the iOS Keychain, never to the config file.")
            }
        }
        .navigationTitle("Copy accounts")
        // `.data` rather than a type of our own: the export has no registered
        // extension, and a file that arrived through a share sheet or a chat app
        // may have lost the one it was written with.
        .fileImporter(isPresented: $picking, allowedContentTypes: [.data, .text]) { result in
            take(result)
        }
        .sheet(isPresented: $pasting) {
            PasteExportView(text: $export) { fileName = "pasted" }
        }
    }

    /// Reads the chosen file. A picked URL is security-scoped, so the read has
    /// to happen between the start and stop of that access.
    private func take(_ result: Result<URL, Error>) {
        failure = nil
        do {
            let url = try result.get()
            let scoped = url.startAccessingSecurityScopedResource()
            defer { if scoped { url.stopAccessingSecurityScopedResource() } }
            let data = try Data(contentsOf: url)
            guard let text = String(data: data, encoding: .utf8) else {
                failure = "That file is not a mailbox export."
                return
            }
            export = text
            fileName = url.lastPathComponent
        } catch {
            failure = error.localizedDescription
        }
    }

    private func load() async {
        working = true
        failure = nil
        defer { working = false }
        do {
            try await store.importPairing(text: export, passphrase: passphrase)
            // On success the root view has accounts and shows the mailbox, so
            // there is nothing to dismiss to.
        } catch {
            failure = error.localizedDescription
        }
    }
}

/// A sheet for the paste route, kept apart so the main screen stays a form.
private struct PasteExportView: View {
    @Environment(\.dismiss) private var dismiss
    @Binding var text: String
    var onDone: () -> Void

    @State private var draft = ""

    var body: some View {
        NavigationStack {
            TextEditor(text: $draft)
                .font(.footnote.monospaced())
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .padding(8)
                .navigationTitle("Paste the export")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Cancel") { dismiss() }
                    }
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Use") {
                            text = draft
                            onDone()
                            dismiss()
                        }
                        .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    }
                }
        }
    }
}
