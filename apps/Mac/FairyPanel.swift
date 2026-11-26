import GravitonKit
import SwiftUI

/// Fairy's eye mark (matching her avatar ring + iris + pupil).
struct FairyEyeMark: View {
    var size: CGFloat = 18
    var color: Color = G.violet

    var body: some View {
        Canvas { ctx, dim in
            let s = min(dim.width, dim.height)
            let scale = s / 24.0
            let center = CGPoint(x: 12 * scale, y: 12 * scale)

            var ring = Path()
            let rOuter = 10.2 * scale
            for i in 0..<4 {
                let base = Angle.degrees(-34 + Double(i) * 90)
                ring.addArc(center: center, radius: rOuter, startAngle: base, endAngle: base + .degrees(62), clockwise: false)
            }
            ctx.stroke(ring, with: .color(color), style: StrokeStyle(lineWidth: 1.7 * scale, lineCap: .round))

            let outerEye = Path(ellipseIn: CGRect(x: 5 * scale, y: 5 * scale, width: 14 * scale, height: 14 * scale))
            ctx.fill(outerEye, with: .color(color))

            let cutout = Path(ellipseIn: CGRect(x: 7.5 * scale, y: 7.9 * scale, width: 9 * scale, height: 9 * scale))
            ctx.fill(cutout, with: .color(G.inspector))

            let pupil = Path(ellipseIn: CGRect(x: 8.9 * scale, y: 9.3 * scale, width: 6.2 * scale, height: 6.2 * scale))
            ctx.fill(pupil, with: .color(color))

            let gleam = Path(ellipseIn: CGRect(x: 12.2 * scale, y: 10.1 * scale, width: 2.2 * scale, height: 2.2 * scale))
            ctx.fill(gleam, with: .color(G.inspector))
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }
}

/// Seamless Stratus right-hand drawer where you talk to Fairy about containers, stacks, images, volumes, and networks.
struct FairyPanel: View {
    @Environment(FairyController.self) private var fairy
    @State private var draft = ""
    @State private var historySearch = ""
    @State private var confirmClearAll = false
    @FocusState private var inputFocused: Bool

    private let starters = [
        "What containers and stacks are running right now?",
        "Show disk usage and reclaimable space",
        "Check the container network and published ports",
        "Any failed containers or recent warnings in logs?",
    ]

    var body: some View {
        VStack(spacing: 0) {
            header

            if fairy.showHistory {
                historyView
            } else {
                conversation
            }

            if let pending = fairy.pendingApproval {
                ApprovalCard(pending: pending)
                    .padding(.horizontal, 14)
                    .padding(.bottom, 10)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
            }

            if !fairy.showHistory {
                composer
            }
        }
        .background(G.inspector)
        .onAppear { inputFocused = true }
    }

    // MARK: Header

    private var header: some View {
        HStack(spacing: 10) {
            FairyEyeMark(size: 20, color: fairy.fairyOnline || fairy.isAnswering ? G.violet : G.textMuted)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(fairy.fairyName)
                        .font(.system(size: 13.5, weight: .semibold))
                        .foregroundStyle(G.textPrimary)
                    GTagPill(fairy.activeHostDisplay, color: G.violet)
                }
                Text(statusLine)
                    .font(.system(size: 11))
                    .foregroundStyle(fairy.pendingApproval != nil ? G.amber : G.textMuted)
                    .lineLimit(1)
            }
            Spacer()

            Button {
                fairy.toggleSpeakReplies()
            } label: {
                Image(systemName: fairy.speakReplies ? "speaker.wave.2.fill" : "speaker.slash")
                    .foregroundStyle(fairy.isSpeaking ? G.violet : G.textMuted)
            }
            .buttonStyle(GIconButtonStyle())
            .help(fairy.speakReplies ? "Mute Fairy's voice replies" : "Speak Fairy's replies aloud")

            Button {
                confirmClearAll = false
                historySearch = ""
                fairy.showHistory.toggle()
            } label: {
                Image(systemName: "clock.arrow.circlepath")
                    .foregroundStyle(fairy.showHistory ? G.violet : G.textMuted)
            }
            .buttonStyle(GIconButtonStyle())
            .help("Past conversations (\(fairy.conversations.count))")

            Button {
                confirmClearAll = false
                historySearch = ""
                fairy.newChat()
            } label: {
                Image(systemName: "square.and.pencil")
            }
            .buttonStyle(GIconButtonStyle())
            .disabled(fairy.messages.isEmpty && !fairy.isAnswering && !fairy.showHistory)
            .help("Start a new conversation")

            Button {
                withAnimation(.snappy(duration: 0.22)) { fairy.isOpen = false }
            } label: {
                Image(systemName: "xmark")
            }
            .buttonStyle(GIconButtonStyle())
            .help("Close Fairy panel (⌥⌘I)")
        }
        .padding(.horizontal, 16)
        .padding(.top, 20)
        .padding(.bottom, 12)
        .windowDraggable()
    }

    private var statusLine: String {
        if fairy.pendingApproval != nil { return "Waiting for Touch ID approval" }
        if fairy.isWaking { return "Waking Fairy in the background…" }
        if fairy.isSpeaking { return "Speaking…" }
        if fairy.isAnswering { return "Working on \(fairy.activeHostDisplay)…" }
        if !fairy.fairyInstalled { return "Fairy.app not found in ~/Applications" }
        return fairy.fairyOnline ? "Connected · Touch ID enabled" : "Ready (starts on first ask)"
    }

    // MARK: Conversation

    private var conversation: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 14) {
                    if fairy.messages.isEmpty {
                        emptyState
                    } else {
                        ForEach(fairy.messages) { msg in
                            messageRow(msg)
                        }
                    }
                    Color.clear.frame(height: 1).id("bottom")
                }
                .padding(16)
            }
            .onChange(of: fairy.messages.count) {
                withAnimation(.easeOut(duration: 0.15)) { proxy.scrollTo("bottom", anchor: .bottom) }
            }
            .onChange(of: fairy.messages.last?.text) {
                proxy.scrollTo("bottom", anchor: .bottom)
            }
            .onChange(of: fairy.messages.last?.steps.count) {
                proxy.scrollTo("bottom", anchor: .bottom)
            }
        }
    }

    private var emptyState: some View {
        VStack(alignment: .leading, spacing: 16) {
            VStack(alignment: .leading, spacing: 8) {
                FairyEyeMark(size: 28, color: G.violet)
                Text("Ask \(fairy.fairyName)")
                    .font(.system(size: 16, weight: .semibold))
                    .foregroundStyle(G.textPrimary)
                Text("Inspect containers, read logs, run commands, manage Compose stacks, build images, or switch between Macs. Destructive or high-impact steps ask for Touch ID.")
                    .font(.system(size: 12.5))
                    .foregroundStyle(G.textMuted)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
            .gravitonCard(G.surfaceSubtle, radius: 12)

            VStack(alignment: .leading, spacing: 6) {
                GCaption("Try asking").padding(.horizontal, 4)
                ForEach(starters, id: \.self) { prompt in
                    Button {
                        fairy.send(prompt)
                    } label: {
                        HStack(spacing: 8) {
                            Image(systemName: "arrow.up.right")
                                .font(.system(size: 10.5, weight: .semibold))
                                .foregroundStyle(G.violet)
                            Text(prompt)
                                .font(.system(size: 12.5))
                                .foregroundStyle(G.body)
                                .multilineTextAlignment(.leading)
                            Spacer(minLength: 0)
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 10)
                        .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                    }
                    .buttonStyle(.plain)
                }
            }

            if !fairy.conversations.isEmpty {
                VStack(alignment: .leading, spacing: 6) {
                    HStack {
                        GCaption("Recent conversations")
                        Spacer()
                        Button("See all (\(fairy.conversations.count))") {
                            confirmClearAll = false
                            historySearch = ""
                            fairy.showHistory = true
                        }
                        .buttonStyle(.plain)
                        .font(.system(size: 11.5, weight: .medium))
                        .foregroundStyle(G.violet)
                    }
                    .padding(.horizontal, 4)

                    ForEach(fairy.conversations.prefix(3)) { conv in
                        Button {
                            fairy.openConversation(conv.id)
                        } label: {
                            HStack(spacing: 8) {
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(conv.title)
                                        .font(.system(size: 12.5, weight: .medium))
                                        .foregroundStyle(G.textPrimary)
                                        .lineLimit(1)
                                    HStack(spacing: 6) {
                                        GTagPill(conv.hostName, color: G.violet)
                                        Text(conv.updatedAt.formatted(.relative(presentation: .named)))
                                            .font(.system(size: 11))
                                            .foregroundStyle(G.textTertiary)
                                    }
                                }
                                Spacer(minLength: 4)
                                Image(systemName: "chevron.right")
                                    .font(.system(size: 10.5, weight: .semibold))
                                    .foregroundStyle(G.textTertiary)
                            }
                            .padding(.horizontal, 12)
                            .padding(.vertical, 9)
                            .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                        }
                        .buttonStyle(.plain)
                    }
                }
            }
        }
        .padding(.top, 6)
    }

    // MARK: History View

    private var filteredConversations: [FairyConversation] {
        FairyHistoryStore.filter(fairy.conversations, query: historySearch)
    }

    private var historyView: some View {
        VStack(spacing: 0) {
            HStack(spacing: 6) {
                GBorderlessSearchField("Search conversations…", text: $historySearch)
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)

            if filteredConversations.isEmpty {
                VStack(spacing: 8) {
                    Spacer()
                    Image(systemName: fairy.conversations.isEmpty ? "bubble.left.and.bubble.right" : "magnifyingglass")
                        .font(.system(size: 22))
                        .foregroundStyle(G.textTertiary)
                    Text(fairy.conversations.isEmpty ? "No past conversations yet" : "No matching conversations")
                        .font(.system(size: 13, weight: .medium))
                        .foregroundStyle(G.textSecondary)
                    Text(fairy.conversations.isEmpty
                        ? "Chats with Fairy are automatically saved here on this Mac."
                        : "Try a different keyword or container name.")
                        .font(.system(size: 12))
                        .foregroundStyle(G.textMuted)
                        .multilineTextAlignment(.center)
                    Spacer()
                }
                .padding(24)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 8) {
                        ForEach(filteredConversations) { conv in
                            ConversationHistoryRow(
                                conversation: conv,
                                isActive: conv.id == fairy.activeConversationID,
                                onOpen: { fairy.openConversation(conv.id) },
                                onCopyTranscript: { fairy.copyTranscript(for: conv.id) },
                                onDelete: { fairy.deleteConversation(conv.id) }
                            )
                        }
                    }
                    .padding(14)
                }
            }

            HStack(spacing: 8) {
                Text("\(fairy.conversations.count) saved")
                    .font(.system(size: 11.5))
                    .foregroundStyle(G.textTertiary)

                Spacer()

                if !fairy.conversations.isEmpty {
                    if confirmClearAll {
                        Button("Cancel") {
                            confirmClearAll = false
                        }
                        .buttonStyle(.g(.ghost, size: .small))

                        Button("Confirm clear") {
                            confirmClearAll = false
                            fairy.clearAllHistory()
                        }
                        .buttonStyle(.g(.dangerSolid, size: .small))
                    } else {
                        Button("Clear all") {
                            confirmClearAll = true
                        }
                        .buttonStyle(.g(.ghost, size: .small))
                    }
                }

                Button("New chat") {
                    confirmClearAll = false
                    historySearch = ""
                    fairy.newChat()
                }
                .buttonStyle(.g(.secondary, size: .small))
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .background(G.surfaceSubtle)
        }
    }

    @ViewBuilder
    private func messageRow(_ msg: FairyChatMessage) -> some View {
        switch msg.role {
        case .user:
            HStack {
                Spacer(minLength: 36)
                Text(msg.text)
                    .font(.system(size: 13))
                    .foregroundStyle(G.textPrimary)
                    .textSelection(.enabled)
                    .padding(.horizontal, 13)
                    .padding(.vertical, 9)
                    .background(G.surfaceActive, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            }

        case .assistant:
            VStack(alignment: .leading, spacing: 8) {
                if !msg.steps.isEmpty {
                    VStack(alignment: .leading, spacing: 4) {
                        ForEach(msg.steps) { step in
                            ToolStepRow(step: step)
                        }
                    }
                }
                if !msg.text.isEmpty {
                    AssistantMessageBubble(text: msg.text)
                } else if fairy.isAnswering && fairy.messages.last?.id == msg.id {
                    HStack(spacing: 8) {
                        GSpinner(size: 11, color: G.violet)
                        Text(fairy.isWaking ? "Opening Fairy…" : "Thinking…")
                            .font(.system(size: 12))
                            .foregroundStyle(G.textMuted)
                    }
                    .padding(.vertical, 2)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)

        case .error:
            VStack(alignment: .leading, spacing: 8) {
                if !msg.steps.isEmpty {
                    VStack(alignment: .leading, spacing: 4) {
                        ForEach(msg.steps) { step in
                            ToolStepRow(step: step)
                        }
                    }
                }
                GStatusLine(.failure, msg.text)
            }
        }
    }

    // MARK: Composer

    private var composer: some View {
        VStack(alignment: .leading, spacing: 8) {
            if !fairy.fairyInstalled {
                Text("Install Fairy.app into ~/Applications to connect.")
                    .font(.system(size: 11.5))
                    .foregroundStyle(G.amber)
            }
            HStack(spacing: 8) {
                GTextField("Ask Fairy to inspect or act…", text: $draft)
                    .focused($inputFocused)
                    .onSubmit { sendDraft() }
                    .disabled(fairy.isAnswering)
                    .gravitonField(height: 36)

                if fairy.isAnswering {
                    Button {
                        fairy.stop()
                    } label: {
                        Image(systemName: "stop.fill")
                    }
                    .buttonStyle(.g(.danger, size: .regular))
                    .help(fairy.isSpeaking ? "Stop speaking" : "Stop Fairy")
                } else {
                    Button {
                        sendDraft()
                    } label: {
                        Image(systemName: "arrow.up")
                    }
                    .buttonStyle(.g(.primary, size: .regular))
                    .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    .help("Send to Fairy")
                }
            }
        }
        .padding(14)
        .background(G.inspector)
    }

    private func sendDraft() {
        let text = draft
        guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, !fairy.isAnswering else { return }
        draft = ""
        fairy.send(text)
    }
}

// MARK: - Subviews

private struct AssistantMessageBubble: View {
    let text: String
    @State private var hovering = false
    @State private var copied = false

    private var formatted: Text {
        if let attr = try? AttributedString(
            markdown: text,
            options: AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        ) {
            return Text(attr)
        }
        return Text(text)
    }

    var body: some View {
        ZStack(alignment: .topTrailing) {
            formatted
                .font(.system(size: 13))
                .foregroundStyle(G.body)
                .tint(G.indigo)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.trailing, 24)

            if hovering || copied {
                Button {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(text, forType: .string)
                    copied = true
                    Task {
                        try? await Task.sleep(for: .seconds(1.2))
                        copied = false
                    }
                } label: {
                    Image(systemName: copied ? "checkmark" : "doc.on.doc")
                        .font(.system(size: 10.5, weight: .medium))
                        .foregroundStyle(copied ? G.mint : G.textMuted)
                        .padding(5)
                        .background(G.surfacePill, in: RoundedRectangle(cornerRadius: 6, style: .continuous))
                }
                .buttonStyle(.plain)
                .help("Copy reply")
                .transition(.opacity)
            }
        }
        .onHover { hovering = $0 }
        .animation(.easeOut(duration: 0.12), value: hovering)
    }
}

private struct ToolStepRow: View {
    var step: FairyToolStep

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 7) {
                stepIcon
                Text(step.title)
                    .font(.system(size: 11.5, weight: .medium))
                    .foregroundStyle(step.status == .denied || step.status == .failed ? G.rose : G.textSecondary)
                    .lineLimit(1)
                Spacer(minLength: 4)
                if step.touchIDVerified {
                    Label("Touch ID", systemImage: "touchid")
                        .font(.system(size: 10, weight: .medium))
                        .foregroundStyle(G.mint)
                }
            }
            if (step.status == .denied || step.status == .failed),
               let note = step.note, !note.isEmpty {
                Text(note)
                    .font(.system(size: 10.5))
                    .foregroundStyle(G.rose.opacity(0.85))
                    .lineLimit(2)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .help(step.note ?? step.tool)
    }

    @ViewBuilder
    private var stepIcon: some View {
        switch step.status {
        case .running:
            GSpinner(size: 10, color: G.violet)
        case .waitingTouchID:
            Image(systemName: "touchid").font(.system(size: 11)).foregroundStyle(G.amber)
        case .done:
            Image(systemName: "checkmark.circle.fill").font(.system(size: 11)).foregroundStyle(G.mint)
        case .denied, .failed:
            Image(systemName: "xmark.circle.fill").font(.system(size: 11)).foregroundStyle(G.rose)
        }
    }
}

private struct ApprovalCard: View {
    @Environment(FairyController.self) private var fairy
    var pending: FairyPendingApproval

    var body: some View {
        let accent = pending.summary.isDestructive ? G.rose : G.amber
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Image(systemName: "touchid")
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(accent)
                VStack(alignment: .leading, spacing: 1) {
                    Text(pending.summary.isDestructive ? "Touch ID Required (Destructive)" : "Touch ID Required")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(accent)
                    Text("Host: \(pending.hostName)")
                        .font(.system(size: 11))
                        .foregroundStyle(G.textMuted)
                }
                Spacer()
            }

            Text(pending.summary.title)
                .font(.system(size: 13.5, weight: .semibold))
                .foregroundStyle(G.textPrimary)

            if !pending.summary.detail.isEmpty {
                Text(pending.summary.detail)
                    .font(G.mono(11.5))
                    .foregroundStyle(G.body)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(10)
                    .background(G.bg, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            }

            if let err = pending.authError {
                Text(err)
                    .font(.system(size: 11.5))
                    .foregroundStyle(G.rose)
            }

            HStack(spacing: 8) {
                Button("Deny", role: .cancel) {
                    fairy.denyPending()
                }
                .buttonStyle(.g(.secondary, size: .regular, fullWidth: true))
                .keyboardShortcut(.cancelAction)

                Button {
                    fairy.approvePendingWithTouchID()
                } label: {
                    if pending.isAuthenticating {
                        Label("Authenticating…", systemImage: fairy.approveButtonSymbol)
                    } else {
                        Label(fairy.approveButtonLabel, systemImage: fairy.approveButtonSymbol)
                    }
                }
                .buttonStyle(.g(pending.summary.isDestructive ? .dangerSolid : .primary, size: .regular, fullWidth: true))
                .disabled(pending.isAuthenticating)
            }
        }
        .padding(14)
        .background(G.palette, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .shadow(color: accent.opacity(0.18), radius: 14, y: 4)
    }
}

private struct ConversationHistoryRow: View {
    let conversation: FairyConversation
    let isActive: Bool
    let onOpen: () -> Void
    let onCopyTranscript: () -> Void
    let onDelete: () -> Void

    @State private var hovered = false

    var body: some View {
        Button(action: onOpen) {
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 6) {
                    if isActive {
                        Circle()
                            .fill(G.violet)
                            .frame(width: 6, height: 6)
                    }
                    Text(conversation.title)
                        .font(.system(size: 13, weight: .semibold))
                        .foregroundStyle(G.textPrimary)
                        .lineLimit(1)
                    Spacer(minLength: 4)
                    if hovered {
                        Button(action: onDelete) {
                            Image(systemName: "trash")
                                .font(.system(size: 11))
                                .foregroundStyle(G.rose)
                        }
                        .buttonStyle(.plain)
                        .help("Delete conversation")
                    } else {
                        Text(conversation.updatedAt.formatted(.relative(presentation: .named)))
                            .font(.system(size: 11))
                            .foregroundStyle(G.textTertiary)
                    }
                }

                Text(conversation.preview)
                    .font(.system(size: 12))
                    .foregroundStyle(G.textMuted)
                    .lineLimit(1)

                HStack(spacing: 6) {
                    GTagPill(conversation.hostName, color: G.violet)
                    Text("\(conversation.messages.count) msgs")
                        .font(.system(size: 11))
                        .foregroundStyle(G.textTertiary)
                    if conversation.stepCount > 0 {
                        Text("· \(conversation.stepCount) steps")
                            .font(.system(size: 11))
                            .foregroundStyle(G.textTertiary)
                    }
                }
            }
            .padding(11)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(isActive ? G.violet.opacity(0.12) : (hovered ? G.surfaceHover : G.surfaceSubtle), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
        }
        .buttonStyle(.plain)
        .onHover { hovered = $0 }
        .contextMenu {
            Button("Open", action: onOpen)
            Button("Copy transcript", action: onCopyTranscript)
            Divider()
            Button("Delete", role: .destructive, action: onDelete)
        }
    }
}
