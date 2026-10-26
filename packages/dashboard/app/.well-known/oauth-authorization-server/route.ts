import { authorizationServerMetadata, oauthErrorResponse } from "@/lib/oauth/server";

export const dynamic = "force-dynamic";

export function GET() {
  try {
    return authorizationServerMetadata();
  } catch (error) {
    return oauthErrorResponse(error);
  }
}
