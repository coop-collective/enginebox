import { oauthErrorResponse, protectedResourceMetadata } from "@/lib/oauth/server";

export const dynamic = "force-dynamic";

export function GET() {
  try {
    return protectedResourceMetadata();
  } catch (error) {
    return oauthErrorResponse(error);
  }
}
