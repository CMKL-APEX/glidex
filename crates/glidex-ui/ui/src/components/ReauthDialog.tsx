import Modal from "./Modal";
import { PamForm, SsoButtons } from "../pages/Login";
import { offersSso, type AuthMethods } from "../types";
import PasskeyButton from "./PasskeyButton";

/** Step-up (401 reauth_required): log in again, then the request is retried. */
export default function ReauthDialog({
  methods,
  username,
  onDone,
  onCancel,
}: {
  methods: AuthMethods;
  username: string;
  onDone: () => void;
  onCancel: () => void;
}) {
  return (
    <Modal title="Confirm it's you" onClose={onCancel}>
      <p className="mb-4 text-sm text-gray-600">
        This action needs a recent login. Sign in again to continue.
      </p>
      {methods.pam && <PamForm initialUsername={username} submitLabel="Sign in and retry" onDone={onDone} />}
      {methods.pam && offersSso(methods) && <div className="my-3" />}
      {offersSso(methods) && <SsoButtons methods={methods} reauth />}
      {methods.passkeys && (
        <div className="mt-3">
          <PasskeyButton onDone={onDone} label="Confirm with a passkey" />
        </div>
      )}
      <button className="mt-3 w-full px-4 py-2 text-sm text-gray-600 hover:bg-gray-100 rounded-lg" onClick={onCancel}>
        Cancel
      </button>
    </Modal>
  );
}
