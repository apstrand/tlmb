import SwiftUI

struct MessageRow: View {
    let message: MessageSummary
    var showAccount = false

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            ZStack {
                Circle()
                    .fill(color(for: message.sender))
                Text(message.from.first?.initials ?? "?")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.white)
            }
            .frame(width: 36, height: 36)
            .overlay(alignment: .topLeading) {
                if message.isUnread {
                    Circle()
                        .fill(.blue)
                        .frame(width: 8, height: 8)
                        .offset(x: -2, y: -2)
                }
            }

            VStack(alignment: .leading, spacing: 2) {
                HStack {
                    Text(message.sender)
                        .font(message.isUnread ? .body.weight(.semibold) : .body)
                        .foregroundStyle(.primary)
                        .lineLimit(1)
                    Spacer()
                    Text(message.when, style: .relative)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                HStack(spacing: 4) {
                    if message.isFlagged {
                        Image(systemName: "flag.fill")
                            .font(.caption2)
                            .foregroundStyle(.orange)
                    }
                    Text(message.subjectLine)
                        .font(message.isUnread ? .subheadline.weight(.medium) : .subheadline)
                        .foregroundStyle(.primary)
                        .lineLimit(1)
                    if message.hasAttachment {
                        Image(systemName: "paperclip")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                }
                if let preview = message.preview, !preview.isEmpty {
                    Text(preview)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                if showAccount {
                    Text(message.account)
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
            }
        }
        .padding(.vertical, 2)
    }

    private func color(for name: String) -> Color {
        let palette: [Color] = [.teal, .indigo, .orange, .pink, .mint, .cyan, .purple, .blue]
        let hash = name.unicodeScalars.reduce(0) { $0 &+ Int($1.value) }
        return palette[abs(hash) % palette.count].opacity(0.85)
    }
}
