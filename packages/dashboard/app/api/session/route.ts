import { signOut } from "@/lib/identity/auth";
import { transport } from "@/lib/identity/mail";
import { errorResponse, json, principal } from "@/lib/server";

export const dynamic = "force-dynamic";

/** Email sign-in is the only way in: POST /api/auth/email, then /api/auth/email/verify. */
export async function GET(request: Request) {
  try {
    const who = principal(request);
    return json({
      authenticated: who !== null,
      me: who && { kind: who.kind, role: who.role, email: who.email, label: who.label, org_id: who.org_id },
      // False when no email gateway (or local file outbox) is configured: codes cannot be delivered.
      email_delivery: transport() !== "none",
    });
  } catch (error) {
    return errorResponse(error);
  }
}

export async function DELETE(request: Request) {
  try {
    return signOut(request);
  } catch (error) {
    return errorResponse(error);
  }
}
