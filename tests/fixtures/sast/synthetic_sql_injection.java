// Synthetic violation: tripped by CodeQL's `java/sql-injection` query
// (CWE-89). This fixture is NOT compiled — it lives outside
// `barback/src/**` so the Java compiler never sees it; the `.java`
// extension is required for CodeQL to identify the language.
//
// The pattern under test: user-controlled string concatenated into a
// SQL statement and executed via JDBC. CodeQL's standard Java query
// pack flags this without any custom rules.

package com.bluminal.barista.barback.fixtures;

import java.sql.Connection;
import java.sql.ResultSet;
import java.sql.Statement;

public class SqlInjectionFixture {
    public ResultSet lookupByName(Connection conn, String userInput) throws Exception {
        Statement st = conn.createStatement();
        // Violation: concatenated SQL; prefer PreparedStatement with
        // bound parameters.
        String sql = "SELECT * FROM artifacts WHERE name = '" + userInput + "'";
        return st.executeQuery(sql);
    }
}
