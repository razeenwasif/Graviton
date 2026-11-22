import GravitonKit
import SwiftUI

/// Fairy's eye mark (matching her avatar ring + iris + pupil).
struct FairyEyeMark: View {
    var size: CGFloat = 18
    var color: Color = G.accent

    var body: some View {
        Canvas { ctx, dim in
            let s = min(dim.width, dim.height)
            let scale = s / 24.0
            let center = CGPoint(x: 12 * scale, y: 12 * scale)

            // Outer broken ring (4 arcs)
            var ring = Path()
            let rOuter = 10.2 * scale
            for i in 0..<4 {
                let base = Angle.degrees(-34 + Double(i) * 90)
                ring.addArc(center: center, radius: rOuter, startAngle: base, endAngle: base + .degrees(62), clockwise: false)
            }
            ctx.stroke(ring, with: .color(color), style: StrokeStyle(lineWidth: 1.7 * scale, lineCap: .round))

            // Outer iris disc
            let outerEye = Path(ellipseIn: CGRect(x: 5 * scale, y: 5 * scale, width: 14 * scale, height: 14 * scale))
            ctx.fill(outerEye, with: .color(color))

            // Inner dark ring
            let cutout = Path(ellipseIn: CGRect(x: 7.5 * scale, y: 7.9 * scale, width: 9 * scale, height: 9 * scale))
            ctx.fill(cutout, with: .color(G.sidebar))

            // Pupil disc
            let pupil = Path(ellipseIn: CGRect(x: 8.9 * scale, y: 9.3 * scale, width: 6.2 * scale, height: 6.2 * scale))
            ctx.fill(pupil, with: .color(color))

            // Specular highlight dot
            let gleam = Path(ellipseIn: CGRect(x: 12.2 * scale, y: 10.1 * scale, width: 2.2 * scale, height: 2.2 * scale))
            ctx.fill(gleam, with: .color(G.sidebar))
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }
}

/// Side panel where you talk to Fairy about containers, stacks, images, volumes, and networks,
/// approve sensitive steps with Touch ID, and see her live steps and replies.
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
            Rectangle().fill(G.border).frame(height: 1)

            if fairy.showHistory {
                historyView
            } else {
                conversation
            }

            if let pending = fairy.pendingApproval {
                Rectangle().fill(G.border).frame(height: 1)
                ApprovalCard(pending: pending)
                    .padding(12)
                    .background(G.surface)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
            }

            if !fairy.showHistory {
                Rectangle().fill(G.border).frame(height: 1)
                composer
            }
        }
        .background(G.sidebar)
        .onAppear { inputFocused = true }
    }

    // MARK: Header

    private var header: some View {
        HStack(spacing: 10) {
            FairyEyeMark(size: 20, color: fairy.fairyOnline || fairy.isAnswering ? G.accent : G.muted)
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 6) {
                    Text(fairy.fairyName)
                        .font(.system(size: 13.5, weight: .semibold))
                        .foregroundStyle(G.text)
                    GBadge(fairy.activeHostDisplay, color: G.accent)
                }
                Text(statusLine)
                    .font(.system(size: 11))
                    .foregroundStyle(fairy.pendingApproval != nil ? G.warning : G.faint)
                    .lineLimit(1)
            }
            Spacer()

            Button {
                fairy.toggleSpeakReplies()
            } label: {
                Image(systemName: fairy.speakReplies ? "speaker.wave.2.fill" : "speaker.slash")
                    .foregroundStyle(fairy.isSpeaking ? G.accent : G.muted)
            }
            .buttonStyle(GIconButtonStyle())
            .help(fairy.speakReplies ? "Mute Fairy's voice replies" : "Speak Fairy's replies aloud")

            Button {
                confirmClearAll = false
                historySearch = ""
                fairy.showHistory.toggle()
            } label: {
                Image(systemName: "clock.arrow.circlepath")
                    .foregroundStyle(fairy.showHistory ? G.accent : G.muted)
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
        .padding(.top, 18)
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
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 8) {
                FairyEyeMark(size: 30, color: G.accent)
                Text("Ask \(fairy.fairyName)")
                    .font(.system(size: 16, weight: .semibold))
                    .foregroundStyle(G.text)
                Text("Inspect containers, read logs, run commands, manage Compose stacks, build images, or switch between Macs. Destructive or high-impact steps ask for Touch ID.")
                    .font(.system(size: 12.5))
                    .foregroundStyle(G.muted)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
            .gravitonCard(G.card, radius: 12)

            VStack(alignment: .leading, spacing: 6) {
                GCaption("Try asking").padding(.horizontal, 4)
                ForEach(starters, id: \.self) { prompt in
                    Button {
                        fairy.send(prompt)
                    } label: {
                        HStack(spacing: 8) {
                            Image(systemName: "arrow.up.right")
                                .font(.system(size: 10.5, weight: .semibold))
                                .foregroundStyle(G.accent)
                            Text(prompt)
                                .font(.system(size: 12.5))
                                .foregroundStyle(G.textSecondary)
                                .multilineTextAlignment(.leading)
                            Spacer(minLength: 0)
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 9)
                        .background(G.card, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                        .overlay(RoundedRectangle(cornerRadius: 9, style: .continuous).strokeBorder(G.border))
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
                        .foregroundStyle(G.accent)
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
                                        .foregroundStyle(G.text)
                                        .lineLimit(1)
                                    HStack(spacing: 6) {
                                        GBadge(conv.hostName, color: G.accent)
                                        Text(conv.updatedAt.formatted(.relative(presentation: .named)))
                                            .font(.system(size: 11))
                                            .foregroundStyle(G.faint)
                                    }
                                }
                                Spacer(minLength: 4)
                                Image(systemName: "chevron.right")
                                    .font(.system(size: 10.5, weight: .semibold))
                                    .foregroundStyle(G.faint)
                            }
                            .padding(.horizontal, 12)
                            .padding(.vertical, 8)
                            .background(G.card, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                            .overlay(RoundedRectangle(cornerRadius: 9, style: .continuous).strokeBorder(G.border))
                        }
                        .buttonStyle(.plain)
                    }
                }
            }
        }
        .padding(.top, 8)
    }

    // MARK: History View

    private var filteredConversations: [FairyConversation] {
        FairyHistoryStore.filter(fairy.conversations, query: historySearch)
    }

    private var historyView: some View {
        VStack(spacing: 0) {
            HStack(spacing: 6) {
                GTextField("Search conversations…", text: $historySearch)
                    .gravitonField(height: 30)
                if !historySearch.isEmpty {
                    Button {
                        historySearch = ""
                    } label: {
                        Image(systemName: "xmark.circle.fill")
                            .foregroundStyle(G.faint)
                    }
                    .buttonStyle(.plain)
                    .help("Clear search")
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)

            Rectangle().fill(G.border).frame(height: 1)

            if filteredConversations.isEmpty {
                VStack(spacing: 8) {
                    Spacer()
                    Image(systemName: fairy.conversations.isEmpty ? "bubble.left.and.bubble.right" : "magnifyingglass")
                        .font(.system(size: 22))
                        .foregroundStyle(G.faint)
                    Text(fairy.conversations.isEmpty ? "No past conversations yet" : "No matching conversations")
                        .font(.system(size: 13, weight: .medium))
                        .foregroundStyle(G.textSecondary)
                    Text(fairy.conversations.isEmpty
                        ? "Chats with Fairy are automatically saved here on this Mac."
                        : "Try a different keyword or container name.")
                        .font(.system(size: 12))
                        .foregroundStyle(G.muted)
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

            Rectangle().fill(G.border).frame(height: 1)

            HStack(spacing: 8) {
                Text("\(fairy.conversations.count) saved")
                    .font(.system(size: 11.5))
                    .foregroundStyle(G.faint)

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
            .background(G.sidebar)
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
                    .foregroundStyle(G.text)
                    .textSelection(.enabled)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 9)
                    .background(G.accent.opacity(0.18), in: RoundedRectangle(cornerRadius: 11, style: .continuous))
                    .overlay(RoundedRectangle(cornerRadius: 11, style: .continuous).strokeBorder(G.accent.opacity(0.3)))
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
                        GSpinner(size: 11)
                        Text(fairy.isWaking ? "Opening Fairy…" : "Thinking…")
                            .font(.system(size: 12))
                            .foregroundStyle(G.muted)
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
                    .foregroundStyle(G.warning)
            }
            HStack(spacing: 8) {
                GTextField("Ask Fairy to inspect or act…", text: $draft)
                    .focused($inputFocused)
                    .onSubmit { sendDraft() }
                    .disabled(fairy.isAnswering)
                    .gravitonField(height: 34)

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
        .padding(12)
        .background(G.sidebar)
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
                .foregroundStyle(G.textSecondary)
                .tint(G.accent)
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
                        .foregroundStyle(copied ? G.running : G.muted)
                        .padding(5)
                        .background(G.card, in: RoundedRectangle(cornerRadius: 6, style: .continuous))
                        .overlay(RoundedRectangle(cornerRadius: 6, style: .continuous).strokeBorder(G.border))
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
                    .foregroundStyle(step.status == .denied || step.status == .failed ? G.danger : G.textSecondary)
                    .lineLimit(1)
                Spacer(minLength: 4)
                if step.touchIDVerified {
                    Label("Touch ID", systemImage: "touchid")
                        .font(.system(size: 10, weight: .medium))
                        .foregroundStyle(G.running)
                }
            }
            if (step.status == .denied || step.status == .failed),
               let note = step.note, !note.isEmpty {
                Text(note)
                    .font(.system(size: 10.5))
                    .foregroundStyle(G.danger.opacity(0.85))
                    .lineLimit(2)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 5)
        .background(G.card, in: RoundedRectangle(cornerRadius: 7, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 7, style: .continuous).strokeBorder(G.border))
        .help(step.note ?? step.tool)
    }

    @ViewBuilder
    private var stepIcon: some View {
        switch step.status {
        case .running:
            GSpinner(size: 10)
        case .waitingTouchID:
            Image(systemName: "touchid").font(.system(size: 11)).foregroundStyle(G.warning)
        case .done:
            Image(systemName: "checkmark.circle.fill").font(.system(size: 11)).foregroundStyle(G.running)
        case .denied, .failed:
            Image(systemName: "xmark.circle.fill").font(.system(size: 11)).foregroundStyle(G.danger)
        }
    }
}

private struct ApprovalCard: View {
    @Environment(FairyController.self) private var fairy
    var pending: FairyPendingApproval

    var body: some View {
        let accent = pending.summary.isDestructive ? G.danger : G.warning
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
                        .foregroundStyle(G.muted)
                }
                Spacer()
            }

            Text(pending.summary.title)
                .font(.system(size: 13.5, weight: .semibold))
                .foregroundStyle(G.text)

            if !pending.summary.detail.isEmpty {
                Text(pending.summary.detail)
                    .font(G.mono(11.5))
                    .foregroundStyle(G.textSecondary)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(8)
                    .background(G.console, in: RoundedRectangle(cornerRadius: 7, style: .continuous))
            }

            if let err = pending.authError {
                Text(err)
                    .font(.system(size: 11.5))
                    .foregroundStyle(G.danger)
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
        .background(G.card, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(accent.opacity(0.45), lineWidth: 1.2))
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
                            .fill(G.accent)
                            .frame(width: 6, height: 6)
                    }
                    Text(conversation.title)
                        .font(.system(size: 13, weight: .semibold))
                        .foregroundStyle(G.text)
                        .lineLimit(1)
                    Spacer(minLength: 4)
                    if hovered {
                        Button(action: onDelete) {
                            Image(systemName: "trash")
                                .font(.system(size: 11))
                                .foregroundStyle(G.danger)
                        }
                        .buttonStyle(.plain)
                        .help("Delete conversation")
                    } else {
                        Text(conversation.updatedAt.formatted(.relative(presentation: .named)))
                            .font(.system(size: 11))
                            .foregroundStyle(G.faint)
                    }
                }

                Text(conversation.preview)
                    .font(.system(size: 12))
                    .foregroundStyle(G.muted)
                    .lineLimit(1)

                HStack(spacing: 6) {
                    GBadge(conversation.hostName, color: G.accent)
                    Text("\(conversation.messages.count) msgs")
                        .font(.system(size: 11))
                        .foregroundStyle(G.faint)
                    if conversation.stepCount > 0 {
                        Text("· \(conversation.stepCount) steps")
                            .font(.system(size: 11))
                            .foregroundStyle(G.faint)
                    }
                }
            }
            .padding(11)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(isActive ? G.accent.opacity(0.1) : G.card, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .strokeBorder(isActive ? G.accent.opacity(0.45) : G.border)
            )
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
