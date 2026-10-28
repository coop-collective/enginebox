import { signOut } from "@/lib/identity/auth";
import { identityEnabled } from "@/lib/identity/store";
import { errorResponse, json, login, principal } from "@/lib/server";

export const dynamic = "force-dynamic";

export async function GET(request: Request) {
  try {
    const who = principal(request);
    return json({
      authenticated: who !== null,
      // Email sign-in is offered when orgs are enabled; the operator password stays as break-glass.
      email_sign_in: identityEnabled(),
      me: who && { kind: who.kind, role: who.role, email: who.email, label: who.label, org_id: who.org_id },
    });
  } catch (error) {
    return errorResponse(error);
  }
}

export async function POST(request: Request) {
  try {
    return await login(request);
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
