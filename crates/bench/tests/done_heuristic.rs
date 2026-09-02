//! A loop's closing line decides its status, so the reading of it must be sturdy.

use bench::loops::says_done;

#[test]
fn done_means_finished_and_negations_do_not() {
    for yes in ["done", "Done.", "All ten invoices are sent. done", "I am done here"] {
        assert!(says_done(yes), "{yes}");
    }
    for no in [
        "I am not done yet",
        "nothing is done",
        "I cannot complete this; the receipts are not done",
        "unable to finish, so this is not done",
        "abandoned",
        "I have done nothing",
        "undone",
    ] {
        assert!(!says_done(no), "{no}");
    }
}
