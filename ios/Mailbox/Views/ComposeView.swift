import SwiftUI

struct ComposeView: View {
    @Environment(\.dismiss) private var dismiss
    @Environment(MailboxStore.self) private var store

    var replyTo: String? = nil
    var mode: ComposeMode = .new

    enum ComposeMode { case new, reply, replyAll, forward }

    @State private var to = ""
    @State private var cc = ""
    @State private var subject = ""
    @State private var messageBody = ""
    @State private var inReplyTo: String?
    @State private var references: [String] = []
    @State private var sending = false
    @State private var showCc = false

    var body: some View {
        NavigationStack {
            Form {
                TextField("To", text: $to)
                    .textInputAutocapitalization(.never)
                    .keyboardType(.emailAddress)
                    .autocorrectionDisabled()
                if showCc || !cc.isEmpty {
                    TextField("Cc", text: $cc)
                        .textInputAutocapitalization(.never)
                        .keyboardType(.emailAddress)
                        .autocorrectionDisabled()
                }
                TextField("Subject", text: $subject)
                TextEditor(text: $messageBody)
                    .frame(minHeight: 220)
                    .font(.body)
            }
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button {
                        Task { await send() }
                    } label: {
                        if sending { ProgressView() } else { Text("Send") }
                    }
                    .disabled(sending || to.trimmingCharacters(in: .whitespaces).isEmpty)
                }
                ToolbarItem(placement: .topBarTrailing) {
                    if !showCc {
                        Button("Cc") { showCc = true }
                    }
                }
            }
            .task { await prefill() }
        }
    }

    private var title: String {
        switch mode {
        case .new: "New message"
        case .reply, .replyAll: "Reply"
        case .forward: "Forward"
        }
    }

    private func prefill() async {
        guard let replyTo else { return }
        do {
            let draft = try MailboxClient.shared.replyDraft(
                id: replyTo,
                all: mode == .replyAll,
                forward: mode == .forward
            )
            to = draft.to.map(\.address).joined(separator: ", ")
            cc = draft.cc.map(\.address).joined(separator: ", ")
            showCc = !cc.isEmpty
            subject = draft.subject
            messageBody = draft.body
            inReplyTo = draft.inReplyTo
            references = draft.references
        } catch {
            store.lastError = error.localizedDescription
        }
    }

    private func send() async {
        sending = true
        defer { sending = false }
        let toList = to.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
        let ccList = cc.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
        do {
            try MailboxClient.shared.send(
                to: toList,
                subject: subject,
                body: messageBody,
                cc: ccList,
                inReplyTo: inReplyTo,
                references: references
            )
            dismiss()
            await store.refresh()
        } catch {
            store.lastError = error.localizedDescription
        }
    }
}
