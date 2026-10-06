import SwiftUI

struct SetupView: View {
    @Environment(MailboxStore.self) private var store

    /// The ports a server almost always listens on, shown as placeholders so
    /// the fields can be left empty and still be right.
    private static let defaultIMAPPort = "993"
    private static let defaultSMTPPort = "587"

    @State private var address = ""
    @State private var password = ""
    @State private var imapHost = ""
    @State private var imapPort = ""
    @State private var imapUser = ""
    @State private var imapSecurity: String?
    @State private var smtpHost = ""
    @State private var smtpPort = ""
    @State private var smtpUser = ""
    @State private var smtpSecurity: String?
    /// One login for both servers is the overwhelmingly common case, so it is
    /// the default and the SMTP field only appears when it is turned off.
    @State private var sameUser = true
    @State private var knownProvider = false
    @State private var provenance: String?
    /// Whether the server section is on screen. The source of truth for the
    /// toolbar button, so pressing it always does something visible.
    @State private var showServers = true
    @State private var discovering = false
    @State private var working = false

    private var domain: String {
        address.split(separator: "@").dropFirst().first.map(String.init) ?? ""
    }

    private var canDiscover: Bool {
        !discovering && domain.contains(".")
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField(
                        "Address",
                        text: $address,
                        // A plain title doubles as the placeholder and picks up
                        // the field's tint; an explicit prompt is what gets the
                        // muted colour placeholder text is supposed to have.
                        prompt: Text("you@example.com").foregroundStyle(.secondary)
                    )
                    .labelsHidden()
                    .textInputAutocapitalization(.never)
                    .keyboardType(.emailAddress)
                    .textContentType(.emailAddress)
                    .autocorrectionDisabled()
                    .onChange(of: address) { lookupProvider() }
                    SecureField(
                        "Password",
                        text: $password,
                        prompt: Text("Password").foregroundStyle(.secondary)
                    )
                    .labelsHidden()
                    .textContentType(.password)
                } header: {
                    Text("Account")
                } footer: {
                    VStack(alignment: .leading, spacing: 4) {
                        if let provenance {
                            Text(provenance)
                        }
                        Text("The password is stored in the iOS Keychain, never in the config file.")
                    }
                }

                if showServers {
                    Section {
                        LabeledContent("Host") {
                            TextField(
                                "IMAP host",
                                text: $imapHost,
                                prompt: Text(placeholderHost("imap")).foregroundStyle(.secondary)
                            )
                            .labelsHidden()
                            .multilineTextAlignment(.trailing)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        }
                        LabeledContent("Port") {
                            TextField(
                                "IMAP port",
                                text: $imapPort,
                                prompt: Text(Self.defaultIMAPPort).foregroundStyle(.secondary)
                            )
                            .labelsHidden()
                            .multilineTextAlignment(.trailing)
                            .keyboardType(.numberPad)
                        }
                        LabeledContent("Username") {
                            TextField(
                                "IMAP username",
                                text: $imapUser,
                                prompt: Text(address.isEmpty ? "Your address" : address)
                                    .foregroundStyle(.secondary)
                            )
                            .labelsHidden()
                            .multilineTextAlignment(.trailing)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        }
                    } header: {
                        Text("IMAP")
                    } footer: {
                        Text("Leave a field empty to use what is shown.")
                    }

                    Section {
                        LabeledContent("Host") {
                            TextField(
                                "SMTP host",
                                text: $smtpHost,
                                prompt: Text(placeholderHost("smtp")).foregroundStyle(.secondary)
                            )
                            .labelsHidden()
                            .multilineTextAlignment(.trailing)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        }
                        LabeledContent("Port") {
                            TextField(
                                "SMTP port",
                                text: $smtpPort,
                                prompt: Text(Self.defaultSMTPPort).foregroundStyle(.secondary)
                            )
                            .labelsHidden()
                            .multilineTextAlignment(.trailing)
                            .keyboardType(.numberPad)
                        }
                        Toggle("Same username as IMAP", isOn: $sameUser)
                        if !sameUser {
                            LabeledContent("Username") {
                                TextField(
                                    "SMTP username",
                                    text: $smtpUser,
                                    prompt: Text(address.isEmpty ? "Your address" : address)
                                        .foregroundStyle(.secondary)
                                )
                                .labelsHidden()
                                .multilineTextAlignment(.trailing)
                                .textInputAutocapitalization(.never)
                                .autocorrectionDisabled()
                            }
                        }
                    } header: {
                        Text("SMTP")
                    }

                    Section {
                        Button {
                            Task { await discover() }
                        } label: {
                            HStack {
                                if discovering {
                                    ProgressView()
                                    Text("Looking up \(domain)")
                                        .foregroundStyle(.secondary)
                                } else {
                                    Label("Discover servers", systemImage: "magnifyingglass")
                                }
                            }
                        }
                        .disabled(!canDiscover)
                    } footer: {
                        Text("Asks \(domain.isEmpty ? "the domain" : domain) for its SRV and MX records.")
                    }
                }

                Section {
                    Button {
                        Task { await add() }
                    } label: {
                        HStack {
                            Spacer()
                            if working { ProgressView() }
                            else { Text("Add account") }
                            Spacer()
                        }
                    }
                    .disabled(working || address.isEmpty || password.isEmpty)
                }
            }
            .navigationTitle("mailbox")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button(showServers ? "Hide servers" : "Servers") {
                        withAnimation { showServers.toggle() }
                    }
                }
            }
        }
    }

    /// What an empty host field will fall back to: whatever discovery found,
    /// else the conventional name under the typed domain.
    private func placeholderHost(_ service: String) -> String {
        domain.isEmpty ? "\(service).example.com" : "\(service).\(domain)"
    }

    /// The instant, offline table lookup, run on every keystroke.
    private func lookupProvider() {
        var hit: ProviderHints?
        if address.contains("@"),
           let hints = try? MailboxClient.shared.providers(address: address), hints.known {
            hit = hints
        }
        let known = hit != nil
        // Fold the server section away when the table answers, and bring it
        // back when it stops answering — but only on the keystroke where that
        // changes, so this is new information rather than a fight with someone
        // who just pressed the toolbar button.
        if known != knownProvider {
            withAnimation { showServers = !known }
        }
        knownProvider = known
        if let hit {
            apply(hit)
        } else {
            provenance = nil
        }
    }

    /// The network lookup, behind the button. Off the main actor because it
    /// blocks on DNS.
    private func discover() async {
        discovering = true
        defer { discovering = false }
        let target = address.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            let discovery = try await Task.detached {
                try MailboxClient.shared.discover(address: target)
            }.value
            // The address may have moved on while the network was busy.
            guard target == address.trimmingCharacters(in: .whitespacesAndNewlines) else { return }
            guard let found = discovery.found else {
                provenance = "Nothing answered for \(domain). Enter the servers below."
                return
            }
            apply(found)
        } catch {
            store.lastError = error.localizedDescription
        }
    }

    /// Fills the server fields in from a lookup.
    ///
    /// Unconditional: both callers are answering a question about the address
    /// as it now stands, so a host left over from a previous domain is stale,
    /// not something to preserve. Anything here can still be typed over.
    private func apply(_ hints: ProviderHints) {
        if let host = hints.imapHost { imapHost = host }
        if let port = hints.imapPort { imapPort = String(port) }
        imapSecurity = hints.imapSecurity ?? imapSecurity
        if let host = hints.smtpHost { smtpHost = host }
        if let port = hints.smtpPort { smtpPort = String(port) }
        smtpSecurity = hints.smtpSecurity ?? smtpSecurity
        provenance = hints.provenance
    }

    /// Fills the server fields in from a network discovery result. A login the
    /// autoconfig names, when it is not the address, goes in the field as a
    /// suggestion rather than straight into the account — and if SMTP names a
    /// different one, the shared-login toggle has to come off to show it.
    private func apply(_ found: DiscoveredServers) {
        imapHost = found.imapHost
        imapPort = String(found.imapPort)
        imapSecurity = found.imapSecurity
        smtpHost = found.smtpHost
        smtpPort = String(found.smtpPort)
        smtpSecurity = found.smtpSecurity
        if let user = found.imapUser, !user.isEmpty { imapUser = user }
        if let user = found.smtpUser, !user.isEmpty, user != found.imapUser {
            smtpUser = user
            sameUser = false
        }
        provenance = found.verified
            ? "Found \(found.imapHost), and it answered."
            : "Found \(found.imapHost)."
    }

    private func add() async {
        working = true
        defer { working = false }
        let trimmed = address.trimmingCharacters(in: .whitespacesAndNewlines)
        // An empty field means "the placeholder", so resolve the two together
        // rather than sending a host with no port or a port with no host.
        let imap = imapHost.isEmpty ? placeholderHost("imap") : imapHost
        let smtp = smtpHost.isEmpty ? placeholderHost("smtp") : smtpHost
        do {
            try MailboxClient.shared.open()
            try await store.addAccount(
                address: trimmed,
                password: password,
                imapHost: imap,
                imapPort: Int(imapPort) ?? Int(Self.defaultIMAPPort),
                imapSecurity: imapSecurity,
                imapUser: imapUser.isEmpty ? nil : imapUser,
                smtpHost: smtp,
                smtpPort: Int(smtpPort) ?? Int(Self.defaultSMTPPort),
                smtpSecurity: smtpSecurity,
                smtpUser: sameUser ? (imapUser.isEmpty ? nil : imapUser)
                                   : (smtpUser.isEmpty ? nil : smtpUser)
            )
        } catch {
            store.lastError = error.localizedDescription
        }
    }
}
