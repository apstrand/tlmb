import SwiftUI

struct SetupView: View {
    @Environment(MailboxStore.self) private var store

    @State private var address = ""
    @State private var password = ""
    @State private var imapHost = ""
    @State private var imapPort = ""
    @State private var smtpHost = ""
    @State private var smtpPort = ""
    @State private var knownProvider = false
    @State private var showAdvanced = false
    @State private var working = false
    @State private var discovering = false
    /// What discovery said, once it has run: where the servers came from, or
    /// that nothing answered for this domain.
    @State private var discoveryNote: String?

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("you@example.com", text: $address)
                        .textInputAutocapitalization(.never)
                        .keyboardType(.emailAddress)
                        .autocorrectionDisabled()
                        .onChange(of: address) { lookupProvider() }
                        // The table is consulted as you type; the network is
                        // asked once, when the address is finished (§9).
                        .onSubmit { Task { await discoverServers() } }
                    SecureField("Password", text: $password)
                } header: {
                    Text("Account")
                } footer: {
                    if discovering {
                        Text("Looking up \(domainOf(address))…")
                    } else if let note = discoveryNote {
                        Text(note)
                    } else {
                        Text(knownProvider
                             ? "Server settings filled in from the address."
                             : "The password is stored in the iOS Keychain, never in the config file.")
                    }
                }

                if showAdvanced || !knownProvider {
                    Section("IMAP") {
                        TextField("Host", text: $imapHost)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        TextField("Port", text: $imapPort)
                            .keyboardType(.numberPad)
                    }
                    Section("SMTP") {
                        TextField("Host", text: $smtpHost)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        TextField("Port", text: $smtpPort)
                            .keyboardType(.numberPad)
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
                    Button(showAdvanced ? "Hide servers" : "Servers") {
                        showAdvanced.toggle()
                    }
                }
            }
        }
    }

    private func lookupProvider() {
        discoveryNote = nil
        guard address.contains("@") else {
            knownProvider = false
            return
        }
        guard let hints = try? MailboxClient.shared.providers(address: address), hints.known else {
            knownProvider = false
            return
        }
        knownProvider = true
        imapHost = hints.imapHost ?? imapHost
        if let p = hints.imapPort { imapPort = String(p) }
        smtpHost = hints.smtpHost ?? smtpHost
        if let p = hints.smtpPort { smtpPort = String(p) }
    }

    /// §9's full chain, for the domains the built-in table has never heard of.
    ///
    /// Failure is silent on purpose: the fields below are already visible and
    /// typing the servers in is the fallback. What a *failure* must not do is
    /// wipe what the person has already typed, so nothing is overwritten
    /// unless discovery actually found something.
    private func discoverServers() async {
        guard address.contains("@"), !knownProvider, !discovering else { return }
        discovering = true
        defer { discovering = false }

        let typed = address.trimmingCharacters(in: .whitespacesAndNewlines)
        let discovery = try? await Task.detached(priority: .userInitiated) {
            try MailboxClient.shared.discover(address: typed)
        }.value

        // The address may have moved on while the network was busy.
        guard typed == address.trimmingCharacters(in: .whitespacesAndNewlines) else { return }

        guard let found = discovery?.found else {
            discoveryNote = "Nothing found for \(domainOf(typed)). Enter the servers below."
            return
        }
        imapHost = found.imapHost
        imapPort = String(found.imapPort)
        smtpHost = found.smtpHost
        smtpPort = String(found.smtpPort)
        discoveryNote = found.verified
            ? "Found \(found.imapHost), and it answered."
            : "Found \(found.imapHost)."
    }

    private func domainOf(_ address: String) -> String {
        address.split(separator: "@").last.map(String.init) ?? address
    }

    private func add() async {
        working = true
        defer { working = false }
        do {
            try MailboxClient.shared.open()
            try await store.addAccount(
                address: address.trimmingCharacters(in: .whitespacesAndNewlines),
                password: password,
                imapHost: imapHost.isEmpty ? nil : imapHost,
                imapPort: Int(imapPort),
                smtpHost: smtpHost.isEmpty ? nil : smtpHost,
                smtpPort: Int(smtpPort)
            )
        } catch {
            store.lastError = error.localizedDescription
        }
    }
}
