import { ReauthDialog as KitReauthDialog } from "@glidex/ui-kit";
import { ssoProviders } from "../pages/Login";
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
    <KitReauthDialog username={username} pam={methods.pam} sso={offersSso(methods) ? ssoProviders(methods) : null} onDone={onDone} onCancel={onCancel}>
      {methods.passkeys && (
        <div className="mt-3">
          <PasskeyButton onDone={onDone} label="Confirm with a passkey" />
        </div>
      )}
    </KitReauthDialog>
  );
}
