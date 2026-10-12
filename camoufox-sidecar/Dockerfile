FROM python:3.12-slim

# Camoufox ships a patched Firefox build; it needs the same native libraries
# a regular Firefox install needs on Debian, even in headless mode.
RUN apt-get update && apt-get install -y --no-install-recommends \
    libgtk-3-0 libx11-xcb1 libasound2 libdbus-glib-1-2 libxt6 \
    ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 10001 camoufox
WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

# Firefox (and Camoufox) refuses to run its own sandbox as root, so the
# browser binary is fetched as the user that will actually run it, into that
# user's own cache dir -- not root's.
USER camoufox
ENV HOME=/home/camoufox
RUN python -m camoufox fetch

COPY --chown=camoufox:camoufox server.py .

ENV SIDECAR_HOST=0.0.0.0
ENV SIDECAR_PORT=8787
EXPOSE 8787

CMD ["python", "server.py"]
