# Column rules

Every normalizer returns the cleaned text, or None when the value is invalid.

## orders_phone

Phone number. Ignore every character that is not a digit. A leading international prefix 0055 counts as the country code 55. Exactly 11 digits are a national number and get country code 55 in front; 55 followed by 11 digits is already complete. Output + and the complete number; anything else is invalid.

## invoices_phone

Phone number. Ignore every character that is not a digit. A leading international prefix 0044 counts as the country code 44. Exactly 10 digits are a national number and get country code 44 in front; 44 followed by 10 digits is already complete. Output + and the complete number; anything else is invalid.

## refunds_phone

Phone number. Ignore every character that is not a digit. A leading international prefix 001 counts as the country code 1. Exactly 10 digits are a national number and get country code 1 in front; 1 followed by 10 digits is already complete. Output + and the complete number; anything else is invalid.

## carts_phone

Phone number. Ignore every character that is not a digit. A leading international prefix 00351 counts as the country code 351. Exactly 9 digits are a national number and get country code 351 in front; 351 followed by 9 digits is already complete. Output + and the complete number; anything else is invalid.

## coupons_phone

Phone number. Ignore every character that is not a digit. A leading international prefix 0049 counts as the country code 49. Exactly 11 digits are a national number and get country code 49 in front; 49 followed by 11 digits is already complete. Output + and the complete number; anything else is invalid.

## shipments_date

Date. Surrounding spaces are ignored. Accept YYYY-MM-DD, or D/M/YYYY written in day/month/year order (one or two digits for day and month). Output YYYY-MM-DD; dates that do not exist, such as February 30, are invalid.

## returns_date

Date. Surrounding spaces are ignored. Accept YYYY-MM-DD, or D/M/YYYY written in month/day/year order (one or two digits for day and month). Output YYYY-MM-DD; dates that do not exist, such as February 30, are invalid.

## vendors_date

Date. Surrounding spaces are ignored. Accept YYYY-MM-DD, or D/M/YYYY written in day/month/year order (one or two digits for day and month). Output YYYY-MM-DD; dates that do not exist, such as February 30, are invalid.

## payouts_date

Date. Surrounding spaces are ignored. Accept YYYY-MM-DD, or D/M/YYYY written in month/day/year order (one or two digits for day and month). Output YYYY-MM-DD; dates that do not exist, such as February 30, are invalid.

## ledgers_date

Date. Surrounding spaces are ignored. Accept YYYY-MM-DD, or D/M/YYYY written in day/month/year order (one or two digits for day and month). Output YYYY-MM-DD; dates that do not exist, such as February 30, are invalid.

## budgets_amount

Amount. Commas are thousands separators. An amount in parentheses, like (12.50), is negative. Round half up to 0.01 and output a plain decimal string with exactly that many places. Text that is not a finite number is invalid.

## quotes_amount

Amount. Commas are thousands separators. An amount in parentheses, like (12.50), is negative. Round half up to 1 and output a plain decimal string with exactly that many places. Text that is not a finite number is invalid.

## receipts_amount

Amount. Commas are thousands separators. An amount in parentheses, like (12.50), is negative. Round half up to 0.001 and output a plain decimal string with exactly that many places. Text that is not a finite number is invalid.

## tickets_amount

Amount. Commas are thousands separators. An amount in parentheses, like (12.50), is negative. Round half up to 0.01 and output a plain decimal string with exactly that many places. Text that is not a finite number is invalid.

## alerts_amount

Amount. Commas are thousands separators. An amount in parentheses, like (12.50), is negative. Round half up to 0.1 and output a plain decimal string with exactly that many places. Text that is not a finite number is invalid.

## reports_email

Email. Surrounding spaces are ignored. Exactly one @, no inner spaces, a nonempty local part and a domain containing a dot. Output the local part unchanged and the domain in lowercase.

## sessions_email

Email. Surrounding spaces are ignored. Exactly one @, no inner spaces, a nonempty local part and a domain containing a dot. The domain must end in .org. Output the local part unchanged and the domain in lowercase.

## tokens_email

Email. Surrounding spaces are ignored. Exactly one @, no inner spaces, a nonempty local part and a domain containing a dot. The domain must end in .edu. Output the local part unchanged and the domain in lowercase.

## teams_email

Email. Surrounding spaces are ignored. Exactly one @, no inner spaces, a nonempty local part and a domain containing a dot. Output the local part unchanged and the domain in lowercase.

## members_email

Email. Surrounding spaces are ignored. Exactly one @, no inner spaces, a nonempty local part and a domain containing a dot. The domain must end in .net. Output the local part unchanged and the domain in lowercase.

## projects_country

Country. Case and repeated spaces do not matter. Accept a two-letter code from the table or one of these names: brasil, brazil, portugal, united kingdom. Output the two-letter code in uppercase; anything else is invalid.

## tasks_country

Country. Case and repeated spaces do not matter. Accept a two-letter code from the table or one of these names: deutschland, france, germany, new zealand. Output the two-letter code in uppercase; anything else is invalid.

## comments_country

Country. Case and repeated spaces do not matter. Accept a two-letter code from the table or one of these names: canada, japan, south africa. Output the two-letter code in uppercase; anything else is invalid.

## labels_country

Country. Case and repeated spaces do not matter. Accept a two-letter code from the table or one of these names: chile, costa rica, mexico, peru. Output the two-letter code in uppercase; anything else is invalid.

## files_country

Country. Case and repeated spaces do not matter. Accept a two-letter code from the table or one of these names: espana, italy, spain, sri lanka. Output the two-letter code in uppercase; anything else is invalid.

## folders_postcode

Postcode. Remove all whitespace and uppercase letters, then the whole code must match the regular expression [A-Z]\d[A-Z]\d[A-Z]\d; anything else is invalid.

## uploads_postcode

Postcode. Remove all whitespace and uppercase letters, then the whole code must match the regular expression \d{4}[A-Z]{2}; anything else is invalid.

## exports_postcode

Postcode. Remove all whitespace and uppercase letters, then the whole code must match the regular expression [A-Z]{1,2}\d[A-Z\d]?\d[A-Z]{2}; anything else is invalid.

## imports_postcode

Postcode. Remove all whitespace and uppercase letters, then the whole code must match the regular expression [A-Z]\d{4}[A-Z]{3}; anything else is invalid.

## schedules_postcode

Postcode. Remove all whitespace and uppercase letters, then the whole code must match the regular expression \d{3}[A-Z]{2}\d; anything else is invalid.

## reminders_name

Person name. Words are separated by any run of whitespace. Each word, and each part of a hyphenated word, starts with an uppercase letter and continues in lowercase, except these particles, which are lowercase unless they are the first word: da, de, do, dos. A blank name is invalid.

## events_name

Person name. Words are separated by any run of whitespace. Each word, and each part of a hyphenated word, starts with an uppercase letter and continues in lowercase, except these particles, which are lowercase unless they are the first word: van, der, den. A blank name is invalid.

## venues_name

Person name. Words are separated by any run of whitespace. Each word, and each part of a hyphenated word, starts with an uppercase letter and continues in lowercase, except these particles, which are lowercase unless they are the first word: von, zu. A blank name is invalid.

## bookings_name

Person name. Words are separated by any run of whitespace. Each word, and each part of a hyphenated word, starts with an uppercase letter and continues in lowercase, except these particles, which are lowercase unless they are the first word: de, la, del. A blank name is invalid.

## rooms_name

Person name. Words are separated by any run of whitespace. Each word, and each part of a hyphenated word, starts with an uppercase letter and continues in lowercase, except these particles, which are lowercase unless they are the first word: di, da. A blank name is invalid.

## guests_flag

Yes/no flag. Case and surrounding spaces do not matter. These words mean true: yes, y, true, 1; these mean false: no, n, false, 0. Output 'true' or 'false'; any other word is invalid.

## menus_flag

Yes/no flag. Case and surrounding spaces do not matter. These words mean true: sim, s; these mean false: nao, n. Output 'true' or 'false'; any other word is invalid.

## dishes_flag

Yes/no flag. Case and surrounding spaces do not matter. These words mean true: on, enabled; these mean false: off, disabled. Output 'true' or 'false'; any other word is invalid.

## stocks_flag

Yes/no flag. Case and surrounding spaces do not matter. These words mean true: ja, j; these mean false: nein, n. Output 'true' or 'false'; any other word is invalid.

## batches_flag

Yes/no flag. Case and surrounding spaces do not matter. These words mean true: active, a; these mean false: inactive, i. Output 'true' or 'false'; any other word is invalid.
