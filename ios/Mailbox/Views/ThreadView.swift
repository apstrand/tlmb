import SwiftUI

struct ThreadView: View {
    @Environment(MailboxStore.self) private var store
    let root: MessageSummary

    @State private var thread: Thread?
    @State private var open: MessageDetail?
    @State private var composing: ComposeSeed?
    @State private var loading = true

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                if loading && thread == nil {
                    ProgressView()
                        .frame(maxWidth: .infinity, minHeight: 200)
                }
                ForEach(thread?.messages ?? [root]) { summary in
                    let expanded = open?.id == summary.id || (thread?.messages.count == 1)
                    MessageCard(summary: summary, detail: expanded ? open : nil) {
                        Task { await load(id: summary.id) }
                    }
                }
            }
            .padding()
        }
        .background(Color(.systemGroupedBackground))
        .navigationTitle(root.subjectLine)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItemGroup(placement: .topBarTrailing) {
                Button { store.toggleFlagged(root) } label: {
                    Image(systemName: root.isFlagged ? "flag.fill" : "flag")
                }
                Button { composing = ComposeSeed(mode: .reply) } label: {
                    Image(systemName: "arrowshape.turn.up.left")
                }
                .accessibilityLabel("Reply")
                Menu {
                    Button("Reply all") { composing = ComposeSeed(mode: .replyAll) }
                    Button("Forward") { composing = ComposeSeed(mode: .forward) }
                    Divider()
                    Button("Archive") { store.archive(root) }
                    Button("Delete", role: .destructive) { store.trash(root) }
                } label: {
                    Image(systemName: "ellipsis.circle")
                }
            }
        }
        .task { await load(id: root.id, asThread: true) }
        .sheet(item: $composing) { seed in
            ComposeView(replyTo: root.id, mode: seed.composeMode)
        }
    }

    private func load(id: String, asThread: Bool = false) async {
        loading = true
        defer { loading = false }
        do {
            try await Task.detached {
                try MailboxClient.shared.ensureBody(id: id)
            }.value
            let shown = try MailboxClient.shared.show(id: id, thread: asThread || thread != nil)
            if let t = shown.thread { thread = t }
            if let m = shown.message {
                open = m
            } else if let first = shown.thread?.messages.last {
                let one = try MailboxClient.shared.show(id: first.id)
                open = one.message
            }
        } catch {
            store.lastError = error.localizedDescription
        }
    }
}

private struct ComposeSeed: Identifiable {
    var mode: Mode
    var id: String { "\(mode)" }
    var composeMode: ComposeView.ComposeMode {
        switch mode {
        case .reply: .reply
        case .replyAll: .replyAll
        case .forward: .forward
        }
    }
    enum Mode { case reply, replyAll, forward }
}

private struct MessageCard: View {
    let summary: MessageSummary
    let detail: MessageDetail?
    var onExpand: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(summary.sender)
                        .font(.subheadline.weight(.semibold))
                    Text(summary.when.formatted(date: .abbreviated, time: .shortened))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if summary.isFlagged {
                    Image(systemName: "flag.fill").foregroundStyle(.orange).font(.caption)
                }
            }
            if let detail, let body = detail.body {
                Text(body.content)
                    .font(.body)
                    .textSelection(.enabled)
                if !detail.attachments.isEmpty {
                    ForEach(detail.attachments.filter(\.isAttachment)) { part in
                        Label(part.filename ?? "part \(part.part)", systemImage: "paperclip")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            } else {
                Text(summary.preview ?? summary.subjectLine)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .contentShape(Rectangle())
                    .onTapGesture(perform: onExpand)
            }
        }
        .padding()
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color(.secondarySystemGroupedBackground), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
    }
}
