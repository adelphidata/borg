# Message Composition (MML)

## Basic Email Structure

Headers followed by a blank line, then the body:

```
From: sender@example.com
To: recipient@example.com
Subject: Hello World

This is the message body.
```

## Headers

- `From:` — Sender address
- `To:` — Primary recipient(s)
- `Cc:` — Carbon copy recipients
- `Bcc:` — Blind carbon copy recipients
- `Subject:` — Message subject
- `Reply-To:` — Address for replies
- `In-Reply-To:` — Message ID being replied to

## Address Formats

```
To: user@example.com
To: John Doe <john@example.com>
To: "John Doe" <john@example.com>
To: user1@example.com, user2@example.com, "Jane" <jane@example.com>
```

## Multipart (Text + HTML)

```
<#multipart type=alternative>
This is the plain text version.
<#part type=text/html>
<html><body><h1>HTML version</h1></body></html>
<#/multipart>
```

## Attachments

```
<#part filename=/path/to/document.pdf><#/part>
```

With custom display name:

```
<#part filename=/path/to/file.pdf name=report.pdf><#/part>
```

## Mixed Content (Text + Attachments)

```
<#multipart type=mixed>
<#part type=text/plain>
Please find the attached files.

Best,
Alice
<#part filename=/path/to/file1.pdf><#/part>
<#part filename=/path/to/file2.zip><#/part>
<#/multipart>
```

## Inline Images

```
<#multipart type=related>
<#part type=text/html>
<html><body>
<p>Check out this image:</p>
<img src="cid:image1">
</body></html>
<#part disposition=inline id=image1 filename=/path/to/image.png><#/part>
<#/multipart>
```

## MML Tag Reference

**`<#multipart>` types:**
- `alternative` — Different representations of same content (text + HTML)
- `mixed` — Independent parts (text + attachments)
- `related` — Parts that reference each other (HTML + images)

**`<#part>` attributes:**
- `type=<mime-type>` — Content type (e.g., `text/html`, `application/pdf`)
- `filename=<path>` — File to attach
- `name=<name>` — Display name for attachment
- `disposition=inline` — Display inline instead of as attachment
- `id=<cid>` — Content ID for referencing in HTML

## CLI Methods

```bash
# Interactive (opens $EDITOR)
himalaya message write

# Prefill headers
himalaya message write -H "To:bob@example.com" -H "Subject:Hello" "Body text"

# From stdin
cat message.txt | himalaya template send

# Reply / Forward (opens $EDITOR with pre-filled template)
himalaya message reply 42
himalaya message reply 42 --all
himalaya message forward 42
```
