import {
  authenticated,
  cookie,
  errorResponse,
  json,
  login,
  requireOrigin,
} from "@/lib/server";

export const dynamic = "force-dynamic";

export async function GET(request: Request) {
  try {
    return json({ authenticated: authenticated(request) });
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
    requireOrigin(request);
    return json({ authenticated: false }, 200, { "Set-Cookie": cookie("", 0) });
  } catch (error) {
    return errorResponse(error);
  }
}
