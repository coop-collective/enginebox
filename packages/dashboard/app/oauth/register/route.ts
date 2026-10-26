import { oauthErrorResponse, register } from "@/lib/oauth/server";

export const dynamic = "force-dynamic";

export async function POST(request: Request) {
  try {
    return await register(request);
  } catch (error) {
    return oauthErrorResponse(error);
  }
}
