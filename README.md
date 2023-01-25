# Invoice2storage

Easy handling for invoices in cooperate environments.

Each user gets a folder accessible for the office. Users can forward their invoices
to a special email address like invoice+bob.allen@example.com

The office has one place where all invoices are collected.

## Operation

This script is used a a email filter to process incoming invoice emails.

1. This script parses emails from stdin or file
2. It determines the user this invoice belongs to
   - if the target email contains a + suffix, the suffix is the user
   - the the from and to domains mach, the sender is the user
3. It tries to  extracts all attachments of certain mime types, defaults to pdf files.
4. It stores the extracted attachments according in the folder specified by template
5. It stores the email in the folder and backend configured

## Configuration

All options are passed as arguments or environment variables if they contain
security related informations.

