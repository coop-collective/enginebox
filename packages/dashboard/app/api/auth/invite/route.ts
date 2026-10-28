import { acceptInvite, invitePage } from "@/lib/identity/auth";
import { errorResponse } from "@/lib/server";

export const dynamic = "force-dynamic";

export function GET(request: Request) {
  try {
    return invitePage(request);
  } catch (error) {
    return errorResponse(error);
  }
}

export async function POST(request: Request) {
  try {
    return await acceptInvite(request);
  } catch (error) {
    return errorResponse(error);
  }
}
