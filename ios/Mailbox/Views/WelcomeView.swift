import SwiftUI

/// First launch, with nothing configured.
///
/// The setup form used to be the first thing on screen, which asks for an IMAP
/// host before saying what the app is or that there is a way not to type any of
/// it. §8's whole promise is that a second device does not re-enter passwords,
/// so the two routes belong side by side at the point where someone has to pick
/// one.
struct WelcomeView: View {
    @State private var addingManually = false

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(spacing: 0) {
                    VStack(spacing: 12) {
                        Image(systemName: "tray.full")
                            .font(.system(size: 52, weight: .light))
                            .foregroundStyle(.tint)
                            .accessibilityHidden(true)
                        Text("mailbox")
                            .font(.largeTitle.weight(.semibold))
                        Text("Your mail, on this device. It talks to your own servers and nowhere else.")
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                            .multilineTextAlignment(.center)
                    }
                    .frame(maxWidth: .infinity)
                    .padding(.top, 48)
                    .padding(.horizontal, 24)

                    VStack(spacing: 12) {
                        Button {
                            addingManually = true
                        } label: {
                            Label("Set up an account", systemImage: "envelope")
                                .frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.borderedProminent)

                        NavigationLink {
                            TransferView()
                        } label: {
                            Label("Copy from another device", systemImage: "arrow.down.doc")
                                .frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.bordered)
                    }
                    .controlSize(.large)
                    .padding(.top, 40)
                    .padding(.horizontal, 24)

                    VStack(alignment: .leading, spacing: 10) {
                        Label {
                            Text("Setting up takes your address and its password. Common providers fill their own servers in.")
                        } icon: {
                            Image(systemName: "1.circle")
                        }
                        Label {
                            Text("Already running mailbox on a computer or another phone? Bring its accounts across instead of typing them again.")
                        } icon: {
                            Image(systemName: "2.circle")
                        }
                    }
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .padding(.top, 36)
                    .padding(.horizontal, 28)
                    .padding(.bottom, 32)
                }
            }
            .sheet(isPresented: $addingManually) {
                SetupView()
            }
        }
    }
}
