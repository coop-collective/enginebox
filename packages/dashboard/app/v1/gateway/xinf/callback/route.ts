import { xinfCallback } from "@/lib/gateway-callback";
import { errorPage } from "@/lib/oauth/pages";

export const dynamic = "force-dynamic";

// A static segment, so it wins over app/v1/[...path] (the bearer-only resource proxy).
export async function GET(request: Request) {
  try {
    return await xinfCallback(request);
  } catch {
    return errorPage("Engine Box could not complete this connection.", 500);
  }
}
