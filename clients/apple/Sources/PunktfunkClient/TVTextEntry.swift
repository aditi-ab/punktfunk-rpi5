// The native tvOS text-entry experience: real tvOS apps never edit text inline —
// selecting a field presents the SYSTEM full-screen keyboard (Apple's "Designing the
// Keyboard Input Experience"). UIKit gives that for free: a UITextField that becomes
// first responder presents the fullscreen keyboard UI with the field's placeholder as
// the prompt. SwiftUI's inline TextField on tvOS is an expanding pill with stray
// chrome — this bridge replaces it everywhere on tvOS.

#if os(tvOS)
import SwiftUI
import UIKit

/// Present inside a fullScreenCover: immediately raises the system keyboard for one
/// value, then calls `onDone` with the result (also on Menu-button dismissal, with
/// whatever was typed so far — match the system apps' "edits stick" behavior).
struct TVTextEntry: UIViewControllerRepresentable {
    let title: String
    let text: String
    var keyboardType: UIKeyboardType = .default
    let onDone: (String) -> Void

    func makeUIViewController(context: Context) -> TVTextEntryController {
        let controller = TVTextEntryController()
        controller.configure(
            title: title, text: text, keyboardType: keyboardType, onDone: onDone)
        return controller
    }

    func updateUIViewController(_ controller: TVTextEntryController, context: Context) {}
}

final class TVTextEntryController: UIViewController, UITextFieldDelegate {
    private let field = UITextField()
    private var onDone: ((String) -> Void)?
    private var finished = false

    func configure(
        title: String, text: String, keyboardType: UIKeyboardType,
        onDone: @escaping (String) -> Void
    ) {
        field.placeholder = title
        field.text = text
        field.keyboardType = keyboardType
        field.returnKeyType = .done
        self.onDone = onDone
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .clear
        field.delegate = self
        view.addSubview(field) // must be in a window to become first responder
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        field.becomeFirstResponder() // presents the tvOS fullscreen keyboard
    }

    func textFieldShouldReturn(_ textField: UITextField) -> Bool {
        textField.resignFirstResponder()
        return true
    }

    func textFieldDidEndEditing(_ textField: UITextField) {
        guard !finished else { return }
        finished = true
        onDone?(textField.text ?? "")
    }
}

/// A Settings-app-style value row: label leading, current value trailing — the whole
/// row is one system lozenge, and pressing it opens the fullscreen keyboard.
struct TVFieldRow: View {
    let label: String
    let value: String
    let placeholder: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack {
                Text(label)
                Spacer()
                Text(value.isEmpty ? placeholder : value)
                    .foregroundStyle(.secondary)
            }
        }
    }
}
#endif
