import Modal from "./Modal";
import { PamForm, SsoButton } from "../pages/Login";
import type { AuthMethods } from "../types";

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
      {methods.pam && methods.oidc && <div className="my-3" />}
      {methods.oidc && <SsoButton reauth />}
      <button className="mt-3 w-full px-4 py-2 text-sm text-gray-600 hover:bg-gray-100 rounded-lg" onClick={onCancel}>
        Cancel
      </button>
    </Modal>
  );
}
