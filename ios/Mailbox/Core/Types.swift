import Foundation

struct MailboxError: Error, LocalizedError, Decodable {
    struct Body: Decodable {
        var code: String
        var message: String
        var retryable: Bool
        var hint: String?
    }

    var error: Body

    var errorDescription: String? { error.message }
    var recoverySuggestion: String? { error.hint }
    var code: String { error.code }
    var retryable: Bool { error.retryable }
}

struct Address: Codable, Hashable, Identifiable {
    var name: String?
    var address: String

    var id: String { address }

    var display: String {
        if let name, !name.trimmingCharacters(in: .whitespaces).isEmpty {
            return name
        }
        return address
    }

    var initials: String {
        let source = display
        let parts = source.split(whereSeparator: { $0 == " " || $0 == "@" })
        let letters = parts.prefix(2).compactMap { $0.first }
        if letters.isEmpty { return "?" }
        return String(letters).uppercased()
    }
}

struct MessageSummary: Codable, Hashable, Identifiable {
    var id: String
    var account: String
    var folder: String
    var threadId: String
    var date: String
    var from: [Address]
    var to: [Address]
    var cc: [Address]
    var subject: String
    var flags: [String]
    var tags: [String]
    var hasAttachment: Bool
    var size: UInt64
    var preview: String?

    var isUnread: Bool { !flags.contains("seen") }
    var isFlagged: Bool { flags.contains("flagged") }

    var sender: String { from.first?.display ?? "(unknown)" }
    var subjectLine: String { subject.isEmpty ? "(no subject)" : subject }

    var parsedDate: Date? {
        Self.dateParser.date(from: date)
    }

    private static let dateParser: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f
    }()

    private static let dateParserPlain: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime]
        return f
    }()

    var when: Date {
        if let d = Self.dateParser.date(from: date) { return d }
        if let d = Self.dateParserPlain.date(from: date) { return d }
        return .distantPast
    }
}

struct Body: Codable, Hashable {
    var format: String
    var content: String
    var truncated: Bool
}

struct Attachment: Codable, Hashable, Identifiable {
    var part: String
    var filename: String?
    var mimeType: String
    var size: UInt64
    var contentId: String?
    var isAttachment: Bool
    var id: String { part }
}

struct MessageDetail: Codable, Hashable, Identifiable {
    var id: String
    var account: String
    var folder: String
    var threadId: String
    var date: String
    var from: [Address]
    var to: [Address]
    var cc: [Address]
    var subject: String
    var flags: [String]
    var tags: [String]
    var hasAttachment: Bool
    var size: UInt64
    var preview: String?
    var messageId: String?
    var inReplyTo: String?
    var references: [String]
    var replyTo: [Address]
    var bcc: [Address]
    var body: Body?
    var attachments: [Attachment]

    var summary: MessageSummary {
        MessageSummary(
            id: id, account: account, folder: folder, threadId: threadId, date: date,
            from: from, to: to, cc: cc, subject: subject, flags: flags, tags: tags,
            hasAttachment: hasAttachment, size: size, preview: preview
        )
    }
}

struct Thread: Codable, Hashable, Identifiable {
    var id: String
    var subject: String
    var messageCount: UInt32
    var unreadCount: UInt32
    var lastDate: String
    var participants: [Address]
    var messages: [MessageSummary]
}

struct Folder: Codable, Hashable, Identifiable {
    var account: String
    var path: String
    var name: String
    var specialUse: String?
    var total: UInt32
    var unread: UInt32
    var selectable: Bool
    var id: String { "\(account):\(path)" }
}

struct Account: Codable, Hashable, Identifiable {
    var id: String
    var address: String
    var loginUser: String
    var imapHost: String
    var imapPort: UInt16
    var hasCredential: Bool
    var lastSync: String?
}

struct SyncReport: Codable, Hashable {
    var account: String
    var foldersExamined: UInt32
    var messagesAdded: UInt32
    var messagesUpdated: UInt32
    var messagesRemoved: UInt32
    var bodiesFetched: UInt32
    var opsApplied: UInt32
    var durationMs: UInt64
}

struct ListReply: Decodable {
    var messages: [MessageSummary]
    var cursor: String?
    var total: UInt64?
}

struct ShowReply: Decodable {
    var message: MessageDetail?
    var thread: Thread?
}

struct AccountsReply: Decodable {
    var accounts: [Account]
}

struct FoldersReply: Decodable {
    var folders: [Folder]
}

struct SyncReply: Decodable {
    var synced: [SyncReport]
}

struct DraftReply: Decodable {
    var from: Address?
    var to: [Address]
    var cc: [Address]
    var bcc: [Address]
    var subject: String
    var body: String
    var inReplyTo: String?
    var references: [String]
}

struct ProviderHints: Decodable {
    var known: Bool
    var imapHost: String?
    var imapPort: UInt16?
    var imapSecurity: String?
    var smtpHost: String?
    var smtpPort: UInt16?
    var smtpSecurity: String?
    /// `table` | `srv` | `mx` | `guess`, present only on a `discover` reply.
    var imapSource: String?
    var smtpSource: String?
    /// Whether the domain published these settings itself, as opposed to their
    /// being assembled from its MX or from the conventional names.
    var authoritative: Bool?

    /// What the setup screen tells the user about where these came from.
    var provenance: String? {
        switch imapSource {
        case "table": "Server settings for this provider are built in."
        case "srv": "Found in the domain's own DNS records."
        case "mx": "Guessed from the domain's mail exchanger — check them."
        case "guess": "No settings published; these are the usual names — check them."
        default: nil
        }
    }
}

/// The result of §9's full discovery chain, as `discover` returns it.
struct Discovery: Decodable {
    var address: String
    var domain: String
    /// `nil` when every source came up empty, which is a domain that needs
    /// its servers typed in rather than an error.
    var found: DiscoveredServers?
    var steps: [DiscoveryStep]
}

struct DiscoveredServers: Decodable {
    var imapHost: String
    var imapPort: UInt16
    var imapSecurity: String
    var smtpHost: String
    var smtpPort: UInt16
    var smtpSecurity: String
    var imapUser: String?
    var smtpUser: String?
    /// Which of §9's sources answered: `built_in`, `autoconfig`, `ispdb`,
    /// `autodiscover`, `srv`, `probe`.
    var source: String
    /// Whether anything actually connected to `imapHost`.
    var verified: Bool
}

struct DiscoveryStep: Decodable {
    var source: String
    var target: String
    var outcome: String
}
