import { oauthErrorResponse, revoke } from "@/lib/oauth/server";

export const dynamic = "force-dynamic";

export async function POST(request: Request) {
  try {
    return await revoke(request);
  } catch (error) {
    return oauthErrorResponse(error);
  }
}
