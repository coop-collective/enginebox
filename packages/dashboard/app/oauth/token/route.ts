import { oauthErrorResponse, token } from "@/lib/oauth/server";

export const dynamic = "force-dynamic";

export async function POST(request: Request) {
  try {
    return await token(request);
  } catch (error) {
    return oauthErrorResponse(error);
  }
}
